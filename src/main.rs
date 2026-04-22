//! 程序入口。
//!
//! 这个文件后续负责：
//! - 初始化应用启动流程
//! - 加载配置与运行参数
//! - 启动 CLI/TUI 主循环
//! - 把顶层控制权交给 `app` 模块

mod agent;
mod app;
mod approval;
mod bus;
mod cli;
mod config;
mod context;
mod fs;
mod git;
mod model;
mod session;
mod shell;
mod tools;

fn main() {
    // 模块骨架阶段暂不放具体启动逻辑。
}
