//! 程序入口。
//!
//! 这个文件后续负责：
//! - 初始化应用启动流程
//! - 加载配置与运行参数
//! - 启动 CLI/TUI 主循环
//! - 把顶层控制权交给 `app` 模块

mod app;
mod cli;
mod config;
mod agent;
mod planner;
mod context;
mod session;
mod model;
mod tools;
mod shell;
mod fs;
mod patch;
mod repo;
mod git;
mod sandbox;
mod approval;
mod memory;
mod telemetry;
mod types;
mod errors;
mod utils;

fn main() {
    // 模块骨架阶段暂不放具体启动逻辑。
}
