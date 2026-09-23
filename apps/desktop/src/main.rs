// 冒烟测试用的最小 Tauri 壳入口;实际逻辑在 lib.rs(P7 正式 UI)。
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    fq_desktop_lib::run()
}
