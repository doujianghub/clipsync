//! 验证 macOS 事件泵：投递一个自定义事件，确认 pump 能取出并派发。
//! 用法：cargo run -p clipsync-app --example pump_check
#[cfg(target_os = "macos")]
fn main() {
    use objc2_app_kit::{NSApplication, NSApplicationActivationPolicy, NSEvent, NSEventMask,
                        NSEventModifierFlags, NSEventSubtype, NSEventType};
    use objc2_foundation::{MainThreadMarker, NSDate, NSDefaultRunLoopMode, NSPoint};

    let mtm = MainThreadMarker::new().expect("必须在主线程");
    let app = NSApplication::sharedApplication(mtm);
    app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);
    app.finishLaunching();

    // 造一个 ApplicationDefined 事件投进队列。
    let ev = NSEvent::otherEventWithType_location_modifierFlags_timestamp_windowNumber_context_subtype_data1_data2(
            NSEventType::ApplicationDefined,
            NSPoint::new(0.0, 0.0),
            NSEventModifierFlags::empty(),
            0.0,
            0,
            None,
            NSEventSubtype::WindowExposed.0,
            42,
            0,
        )
    .expect("构造事件失败");
    app.postEvent_atStart(&ev, true);

    // 用与 tray.rs 相同的方式抽干队列。
    let mode = unsafe { NSDefaultRunLoopMode };
    let mut drained = 0;
    while let Some(e) = app.nextEventMatchingMask_untilDate_inMode_dequeue(
        NSEventMask::Any, Some(&NSDate::distantPast()), mode, true,
    ) {
        drained += 1;
        let _ = e.r#type();
        app.sendEvent(&e);
    }
    println!("drained={drained}");
    assert!(drained >= 1, "事件泵未取出投递的事件——pump 失效");
    println!("PASS: 事件泵可取出并派发事件");

    // 队列空时应立即返回 None，不阻塞。
    let t = std::time::Instant::now();
    let none = app.nextEventMatchingMask_untilDate_inMode_dequeue(
        NSEventMask::Any, Some(&NSDate::distantPast()), mode, true,
    );
    let el = t.elapsed();
    assert!(none.is_none(), "队列应已排空");
    assert!(el.as_millis() < 100, "空队列时不应阻塞，实际 {el:?}");
    println!("PASS: 空队列立即返回（{el:?}），不阻塞主线程");
}

#[cfg(not(target_os = "macos"))]
fn main() { println!("仅适用于 macOS"); }
