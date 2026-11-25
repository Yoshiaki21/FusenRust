//! fusen-server のコマンド。
//!
//!   fusen-server [serve]                           サーバーを起動する（既定）
//!   fusen-server token create --device <端末名>     トークンを発行する（表示は 1 回だけ）
//!   fusen-server token list                         トークンの一覧
//!   fusen-server token revoke <ID>                  トークンを無効にする（ID の先頭だけでもよい）
//!
//! 設定は環境変数（または同名の引数）: FUSEN_LISTEN, FUSEN_DATA_DIR, FUSEN_MAX_UPLOAD_BYTES

use std::net::SocketAddr;
use std::path::PathBuf;

use clap::{Parser, Subcommand};
use fusen_server::{Config, DEFAULT_MAX_UPLOAD_BYTES, Db, db};

#[derive(Parser)]
#[command(name = "fusen-server", version, about = "FusenRust の同期サーバー")]
struct Cli {
    /// 待ち受けるアドレス
    #[arg(
        long,
        env = "FUSEN_LISTEN",
        default_value = "0.0.0.0:8080",
        global = true
    )]
    listen: SocketAddr,
    /// データを置くディレクトリ（fusen.db と blobs/）
    #[arg(long, env = "FUSEN_DATA_DIR", default_value = "./data", global = true)]
    data_dir: PathBuf,
    /// 1 つの blob のアップロード上限（バイト）
    #[arg(long, env = "FUSEN_MAX_UPLOAD_BYTES", default_value_t = DEFAULT_MAX_UPLOAD_BYTES, global = true)]
    max_upload_bytes: u64,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// サーバーを起動する
    Serve,
    /// トークンの管理
    Token {
        #[command(subcommand)]
        action: TokenCommand,
    },
}

#[derive(Subcommand)]
enum TokenCommand {
    /// 新しいトークンを発行する（トークンは 1 回だけ表示される）
    Create {
        /// 端末名（例: cachyos-main）
        #[arg(long)]
        device: String,
    },
    /// トークンの一覧
    List,
    /// トークンを無効にする
    Revoke {
        /// トークンの ID（先頭だけでもよい）
        id: String,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,tower_http=info".into()),
        )
        .init();
    let cli = Cli::parse();
    match cli.command.unwrap_or(Command::Serve) {
        Command::Serve => {
            fusen_server::serve(Config {
                listen: cli.listen,
                data_dir: cli.data_dir,
                max_upload_bytes: cli.max_upload_bytes,
            })
            .await
        }
        Command::Token { action } => {
            let db = Db::open(&cli.data_dir)?;
            match action {
                TokenCommand::Create { device } => {
                    let (id, token) = db.with(|c| db::create_token(c, &device))?;
                    println!("トークンを発行しました（このトークンは二度と表示されません）");
                    println!("  端末: {device}");
                    println!("  ID:   {id}");
                    println!("  トークン: {token}");
                }
                TokenCommand::List => {
                    let tokens = db.with(|c| db::list_tokens(c))?;
                    if tokens.is_empty() {
                        println!("トークンはありません");
                    }
                    println!(
                        "{:<38} {:<20} {:<10} {:<26} 最終使用",
                        "ID", "端末", "状態", "発行"
                    );
                    for t in tokens {
                        let state = if t.revoked_at.is_some() {
                            "無効"
                        } else {
                            "有効"
                        };
                        println!(
                            "{:<38} {:<20} {:<10} {:<26} {}",
                            t.token_id,
                            t.device_name,
                            state,
                            t.created_at,
                            t.last_used_at.unwrap_or_else(|| "-".into())
                        );
                    }
                }
                TokenCommand::Revoke { id } => {
                    if db.with(|c| db::revoke_token(c, &id))? {
                        println!("トークン {id} を無効にしました");
                        // この端末の票を待っていた付箋は、ここで完全に削除される（D-014）
                        let (count, unused) = db
                            .with(db::purge_ready_notes)
                            .map_err(|e| anyhow::anyhow!("{e}"))?;
                        fusen_server::remove_blobs(&cli.data_dir.join("blobs"), &unused);
                        if count > 0 {
                            println!("全端末で削除済みになった付箋 {count} 件を完全に削除しました");
                        }
                    } else {
                        anyhow::bail!("有効なトークン {id} が見つかりません");
                    }
                }
            }
            Ok(())
        }
    }
}
