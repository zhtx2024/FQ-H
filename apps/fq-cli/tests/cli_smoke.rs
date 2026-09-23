//! CLI 端到端验收:启动两个真实的 fq-cli 进程,脚本驱动跑通
//! 发现 → 文本互发 → 送达回执 → 文件传输 → 历史查询 的完整 MVP 链路。

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use tokio::process::Command;

const BIN: &str = env!("CARGO_BIN_EXE_fq-cli");
/// 不可达"广播"目标(测试不触碰真实网卡;发现走 bootstrap 单播)
const DEAD_BROADCAST: SocketAddr =
    SocketAddr::new(std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST), 1);

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir()
        .join("fq-cli-tests")
        .join(format!("{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn write_script(dir: &std::path::Path, name: &str, lines: &[&str]) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, lines.join("\n") + "\n").unwrap();
    path
}

struct CliRun {
    stdout: String,
    stderr: String,
    code: Option<i32>,
}

async fn run_cli(args: Vec<String>) -> CliRun {
    let output = tokio::time::timeout(
        Duration::from_secs(120),
        Command::new(BIN).args(&args).output(),
    )
    .await
    .expect("CLI 进程超时(120s)")
    .expect("CLI 进程启动失败");
    CliRun {
        stdout: String::from_utf8_lossy(&output.stdout).to_string(),
        stderr: String::from_utf8_lossy(&output.stderr).to_string(),
        code: output.status.code(),
    }
}

/// 全局唯一端口,避免与其它测试/残留进程冲突。
fn ports(seed: u16) -> (u16, u16) {
    (26100 + seed * 10, 26101 + seed * 10)
}

#[tokio::test]
async fn two_cli_processes_full_mvp_flow() {
    let a_dir = temp_dir("e2e-a");
    let b_dir = temp_dir("e2e-b");
    let a_send = temp_dir("e2e-send");
    let (a_disc, a_tcp) = ports(1);
    let (b_disc, b_tcp) = ports(2);

    // 源文件:1.2 MiB 伪随机数据(跨多个分块)
    let payload: Vec<u8> = (0..1_258_291u32).map(|i| (i * 31 + 7) as u8).collect();
    let src_file = a_send.join("验收数据.bin");
    std::fs::write(&src_file, &payload).unwrap();

    let b_script = write_script(
        &b_dir,
        "b.txt",
        &[
            "/wait-peer Alice",
            "/expect 你好,来自 CLI 验收",
            "/expect-file 验收数据.bin",
            "/sleep 1",
            "/to Alice",
            "/history 5",
            "/read",
            "/quit",
        ],
    );
    let a_script = write_script(
        &a_dir,
        "a.txt",
        &[
            "/wait-peer Bob",
            "/to Bob",
            "你好,来自 CLI 验收!",
            "/sleep 1",
            "/file 验收数据.bin占位",
            "/sleep 5",
            "/quit",
        ],
    );
    // 修正 file 路径为绝对路径(临时目录无空格,不需要引号)
    let a_script_fixed = a_dir.join("a-fixed.txt");
    let content = std::fs::read_to_string(&a_script)
        .unwrap()
        .replace("验收数据.bin占位", &src_file.display().to_string());
    std::fs::write(&a_script_fixed, content).unwrap();

    let b_download = b_dir.join("downloads");

    // 先起 B(等待 Alice),再起 A(主动发)
    let b_args: Vec<String> = vec![
        "chat".into(),
        "--name".into(),
        "Bob".into(),
        "--data-dir".into(),
        b_dir.display().to_string(),
        "--discovery-port".into(),
        b_disc.to_string(),
        "--listen-port".into(),
        b_tcp.to_string(),
        "--broadcast".into(),
        DEAD_BROADCAST.to_string(),
        "--bootstrap".into(),
        SocketAddr::from(([127, 0, 0, 1], a_disc)).to_string(),
        "--download-dir".into(),
        b_download.display().to_string(),
        "--heartbeat-ms".into(),
        "150".into(),
        "--script".into(),
        b_script.display().to_string(),
    ];
    let a_args: Vec<String> = vec![
        "chat".into(),
        "--name".into(),
        "Alice".into(),
        "--data-dir".into(),
        a_dir.display().to_string(),
        "--discovery-port".into(),
        a_disc.to_string(),
        "--listen-port".into(),
        a_tcp.to_string(),
        "--broadcast".into(),
        DEAD_BROADCAST.to_string(),
        "--bootstrap".into(),
        SocketAddr::from(([127, 0, 0, 1], b_disc)).to_string(),
        "--download-dir".into(),
        a_dir.join("downloads").display().to_string(),
        "--heartbeat-ms".into(),
        "150".into(),
        "--script".into(),
        a_script_fixed.display().to_string(),
    ];

    let b_task = tokio::spawn(run_cli(b_args));
    // 给 B 一点启动时间,再启动 A
    tokio::time::sleep(Duration::from_millis(500)).await;
    let a_run = run_cli(a_args).await;
    let b_run = b_task.await.expect("B 任务 join 失败");

    // 两进程都应成功退出
    assert_eq!(
        a_run.code,
        Some(0),
        "Alice 退出码异常\nstdout:\n{}\nstderr:\n{}",
        a_run.stdout,
        a_run.stderr
    );
    assert_eq!(
        b_run.code,
        Some(0),
        "Bob 退出码异常\nstdout:\n{}\nstderr:\n{}",
        b_run.stdout,
        b_run.stderr
    );

    // 关键链路断言
    assert!(
        a_run.stdout.contains("[PEER] 上线: Bob"),
        "A 应发现 Bob:\n{}",
        a_run.stdout
    );
    assert!(
        b_run.stdout.contains("[PEER] 上线: Alice"),
        "B 应发现 Alice:\n{}",
        b_run.stdout
    );
    assert!(
        a_run.stdout.contains("[SENT] 你好,来自 CLI 验收"),
        "A 应成功发出文本:\n{}",
        a_run.stdout
    );
    assert!(
        b_run.stdout.contains("[RECV] Alice: 你好,来自 CLI 验收"),
        "B 应收到文本:\n{}",
        b_run.stdout
    );
    assert!(
        a_run.stdout.contains("[DELIVERED]") || a_run.stdout.contains("[READ]"),
        "A 应收到送达/已读回执:\n{}",
        a_run.stdout
    );
    assert!(
        b_run.stdout.contains("[FILE] ↓ 传输完成"),
        "B 侧文件传输应完成:\n{}",
        b_run.stdout
    );

    // 文件内容逐字节一致
    let received = std::fs::read(b_download.join("验收数据.bin")).expect("接收文件应存在");
    assert_eq!(received.len(), payload.len());
    assert_eq!(received, payload, "文件内容必须与源一致");

    // B 的历史包含双方消息
    assert!(
        b_run.stdout.contains("对方"),
        "历史应包含对方消息:\n{}",
        b_run.stdout
    );
}
