use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;

use log::{debug, error, info};
use rdev::{Event, EventType, listen};
use serde::Serialize;
use serde_json::{Value, json};
use tauri::{AppHandle, Emitter, Runtime, command};

const DEVICE_CHANGED_EVENT: &str = "device-changed";
/// 系统输入监控权限被拒绝时发出（目前仅 macOS，Windows 无需该权限）
const DEVICE_LISTEN_PERMISSION_EVENT: &str = "device-listen-permission";
/// 监听底层捕获失败时发出（例如 Windows 全局钩子安装失败）
const DEVICE_LISTEN_ERROR_EVENT: &str = "device-listen-error";

/// 串行化的设备事件负载
#[derive(Debug, Clone, Serialize)]
pub enum DeviceEventKind {
    MousePress,
    MouseRelease,
    MouseMove,
    KeyboardPress,
    KeyboardRelease,
}

#[derive(Debug, Clone, Serialize)]
pub struct DeviceEvent {
    kind: DeviceEventKind,
    value: Value,
}

/// 防止并发开启多次监听（rdev 的 GLOBAL_CALLBACK 是全局单例，重复注册会互相覆盖）
static IS_LISTENING: AtomicBool = AtomicBool::new(false);

/// macOS：检测是否已获得「输入监控」权限。
///
/// rdev 底层通过 CGEventTap 接收全局输入，若应用没有输入监控（或辅助功能）权限，
/// 事件会被系统静默丢弃——不会报错，前端表现为「鼠标有反应、键盘无反应」。
/// 这里在启动监听前显式预检一次，未授权时向前端发送事件以便引导授权。
#[cfg(target_os = "macos")]
fn input_monitoring_authorized() -> bool {
    // CGPreflightListenEventAccess 在系统设置中同步返回当前授权状态，不弹窗
    unsafe { CGPreflightListenEventAccess() }
}

#[cfg(target_os = "macos")]
#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGPreflightListenEventAccess() -> bool;
}

#[command]
pub async fn start_device_listening<R: Runtime>(app_handle: AppHandle<R>) -> Result<(), String> {
    if IS_LISTENING.swap(true, Ordering::SeqCst) {
        return Ok(());
    }

    // Windows 上若以非管理员身份运行，将无法捕获以管理员（更高完整性级别）运行的
    // 全屏游戏/进程的输入；该场景在 UI 层已有「以管理员身份运行」引导。
    #[cfg(target_os = "macos")]
    if !input_monitoring_authorized() {
        info!("macOS 输入监控权限未授权，跳过设备监听并提示用户");

        // 未授权时监听必然收不到任何事件，直接返回（并把监听标记复位，
        // 这样用户授权后重启应用即可重新开启监听）。
        IS_LISTENING.store(false, Ordering::SeqCst);

        let _ = app_handle.emit(DEVICE_LISTEN_PERMISSION_EVENT, "input-monitoring");

        return Ok(());
    }

    info!("开始全局设备监听");

    // 放到独立线程运行：Windows 上 rdev 会阻塞在 GetMessageA 消息循环，
    // 直接放在 async worker 上会占满一个 tokio worker。
    thread::spawn(move || {
        let emit_handle = app_handle.clone();
        let callback = move |event: Event| {
            let device_event = match event.event_type {
                EventType::ButtonPress(button) => DeviceEvent {
                    kind: DeviceEventKind::MousePress,
                    value: json!(format!("{:?}", button)),
                },
                EventType::ButtonRelease(button) => DeviceEvent {
                    kind: DeviceEventKind::MouseRelease,
                    value: json!(format!("{:?}", button)),
                },
                EventType::MouseMove { x, y } => DeviceEvent {
                    kind: DeviceEventKind::MouseMove,
                    value: json!({ "x": x, "y": y }),
                },
                EventType::KeyPress(key) => {
                    info!("键盘按下事件: {:?}", key);
                    DeviceEvent {
                        kind: DeviceEventKind::KeyboardPress,
                        value: json!(format!("{:?}", key)),
                    }
                }
                EventType::KeyRelease(key) => {
                    debug!("键盘释放事件: {:?}", key);
                    DeviceEvent {
                        kind: DeviceEventKind::KeyboardRelease,
                        value: json!(format!("{:?}", key)),
                    }
                }
                EventType::Wheel { .. } => return,
            };

            let _ = emit_handle.emit(DEVICE_CHANGED_EVENT, device_event);
        };

        // macOS：rdev 将事件源挂到主 RunLoop 后线程随即退出，事件由主线程持续回调；
        // Windows：此处将长期阻塞，直到应用退出。
        match listen(callback) {
            Ok(()) => {}
            Err(err) => {
                error!("设备监听失败: {:?}", err);

                // 监听失败时复位标记，允许前端稍后重试
                IS_LISTENING.store(false, Ordering::SeqCst);

                let _ = app_handle.emit(
                    DEVICE_LISTEN_ERROR_EVENT,
                    format!("Failed to listen device: {err:?}"),
                );
            }
        }
    });

    Ok(())
}
