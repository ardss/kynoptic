// 看门狗专用入口:windows GUI 子系统,无控制台窗口。
// 计划任务每分钟静默调用:`kynoptic-watchdog.exe watchdog --once`。
// 与 kynoptic / kynoptic-ctl 同源 main.rs,行为一致,只是不再开黑框。
#![windows_subsystem = "windows"]

include!("../main.rs");
