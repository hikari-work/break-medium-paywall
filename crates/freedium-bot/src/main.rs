//! Binary `freedium-bot`.
//!
//! # Tiga subcommand, dan yang ketiga adalah alat verifikasinya
//!
//! `run` adalah botnya. `render` mencetak JSON `InputRichMessage` untuk sebuah
//! URL **tanpa token Telegram** — dan itulah yang membuat seluruh pekerjaan ini
//! bisa diperiksa tanpa mata manusia melihat layar ponsel: JSON-nya dibaca
//! berdampingan dengan halaman web untuk artikel yang sama, dan bedanya terlihat.
//! `healthcheck` adalah denyut yang dipakai compose; lihat
//! [`healthcheck`] soal kenapa ia tidak memeriksa API.
//!
//! # Log ke stderr
//!
//! `render` menulis JSON-nya ke stdout supaya bisa dialirkan ke `jq` atau
//! disimpan ke berkas. `tracing_subscriber` menulis ke stdout secara default,
//! dan satu baris log di tengah JSON membuat keluarannya tidak bisa dibaca
//! mesin. Karena itu seluruh log di binary ini menuju stderr.

use std::process::ExitCode;

use medium_client::http::ReqwestTransport;
use tracing_subscriber::EnvFilter;

use freedium_bot::api;
use freedium_bot::bot::{Bot, RunError};
use freedium_bot::config::Config;
use freedium_bot::rich;
use freedium_bot::telegram;

const USAGE: &str = "\
Freedium Telegram bot

Usage:
  freedium-bot run             Long-poll Telegram until signalled
  freedium-bot render <url>    Print the rich message JSON for a URL
  freedium-bot healthcheck     Ask Telegram whether this bot is up

Options:
  -h, --help   Print this message
";

#[tokio::main]
async fn main() -> ExitCode {
    init_tracing();

    let args: Vec<String> = std::env::args().skip(1).collect();

    match args.first().map(String::as_str) {
        Some("-h" | "--help" | "help") => {
            println!("{USAGE}");
            ExitCode::SUCCESS
        }
        Some("run") => run().await,
        Some("render") => render(args.get(1).map(String::as_str)).await,
        Some("healthcheck") => healthcheck().await,
        Some(other) => {
            eprintln!("unknown command: {other}\n\n{USAGE}");
            ExitCode::from(2)
        }
        None => {
            eprintln!("a command is required\n\n{USAGE}");
            ExitCode::from(2)
        }
    }
}

/// `RUST_LOG` kalau ada, `info` kalau tidak.
///
/// Ke **stderr**, selalu: lihat catatan modul.
fn init_tracing() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));

    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .with_writer(std::io::stderr)
        .init();
}

fn config_or_exit() -> Config {
    match Config::from_env() {
        Ok(config) => config,
        Err(error) => {
            eprintln!("{error}");
            std::process::exit(2);
        }
    }
}

async fn run() -> ExitCode {
    let config = config_or_exit();

    let bot = match Bot::new(&config) {
        Ok(bot) => bot,
        Err(error @ RunError::MissingToken) => {
            eprintln!("{error}");
            return ExitCode::from(2);
        }
    };

    match bot.run(&config, shutdown_signal()).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            // `getMe` di awal loop: kalau ini gagal, tokennya salah atau
            // tidak ada, dan tidak ada gunanya melanjutkan.
            eprintln!("{error}");
            if let Some(advice) = error.advice() {
                eprintln!("{advice}");
            }
            ExitCode::FAILURE
        }
    }
}

/// Mencetak JSON `InputRichMessage` untuk sebuah URL.
///
/// Tanpa token Telegram, dan itu disengaja: ini jalur verifikasi yang harus bisa
/// dipakai sebelum ada bot untuk diuji. Yang dibutuhkannya cuma API-nya sendiri
/// — dan API itu `/api/v1`, yang sudah live.
async fn render(url: Option<&str>) -> ExitCode {
    let Some(url) = url else {
        eprintln!("render butuh sebuah URL\n\n{USAGE}");
        return ExitCode::from(2);
    };

    let config = config_or_exit();
    let transport = ReqwestTransport::new().expect("klien HTTPS langsung selalu bisa dibangun");
    let client = api::Client::new(transport, config.base_url.clone(), config.request_timeout);

    let post_id = match client.resolve(url).await {
        Ok(post_id) => post_id,
        Err(error) => {
            eprintln!("gagal menyelesaikan {url}: {error}");
            return ExitCode::FAILURE;
        }
    };

    let post = match client.post(&post_id).await {
        Ok(post) => post,
        Err(error) => {
            eprintln!("gagal mengambil {post_id}: {error}");
            return ExitCode::FAILURE;
        }
    };

    let rendered = rich::rich_message(&post, client.base_url());

    // Ringkasannya ke stderr supaya stdout tetap JSON murni.
    eprintln!(
        "{post_id}: {} blok, dipotong: {}",
        rendered.message.blocks.len(),
        rendered.truncated
    );

    match serde_json::to_string_pretty(&serde_json::json!({ "rich_message": rendered.message })) {
        Ok(json) => {
            println!("{json}");
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("gagal menyerialkan pesannya: {error}");
            ExitCode::FAILURE
        }
    }
}

/// Satu `getMe`. Nol kalau tokennya masih sah.
///
/// # Kenapa API-nya sengaja **tidak** diperiksa
///
/// Compose menjalankan `freedium_autoheal` dengan `AUTOHEAL_CONTAINER_LABEL=all`,
/// jadi container yang healthcheck-nya gagal akan di-restart berulang. Kalau
/// denyut ini ikut memeriksa `/api/v1`, matinya API — atau Redis, atau tunnel
/// WARP di depannya — akan jadi restart loop untuk bot yang sebenarnya sehat, dan
/// restart tidak pernah memperbaiki dependensi yang sedang down.
///
/// Yang diperiksa karena itu adalah satu-satunya keadaan yang benar-benar milik
/// bot ini dan benar-benar bisa diperbaiki dengan me-restartnya: tokennya masih
/// berlaku. `getMe` juga satu-satunya panggilan yang membuktikan itu — token yang
/// dicabut lewat BotFather tidak terlihat dari dalam proses mana pun.
///
/// Token yang tidak diisi juga bukan kegagalan di sini, dan itu konsisten:
/// healthcheck menjawab "proses ini sehat", bukan "konfigurasinya lengkap".
async fn healthcheck() -> ExitCode {
    let Some(token) = Config::from_env().ok().and_then(|it| it.telegram_token) else {
        eprintln!("TELEGRAM_ARTICLE_BOT_TOKEN belum diisi; tidak ada yang bisa diperiksa");
        return ExitCode::FAILURE;
    };

    let transport = ReqwestTransport::new().expect("klien HTTPS langsung selalu bisa dibangun");
    let telegram = telegram::Client::new(transport, token);

    match telegram.get_me().await {
        Ok(me) => {
            println!(
                "ok: {} (@{})",
                me.first_name,
                me.username.as_deref().unwrap_or("-")
            );
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("{error}");
            if let Some(advice) = error.advice() {
                eprintln!("{advice}");
            }
            ExitCode::FAILURE
        }
    }
}

/// Menunggu sampai prosesnya diminta berhenti.
///
/// `SIGTERM` yang dikirim `docker stop`, dan `SIGINT` dari Ctrl-C — keduanya,
/// karena compose memberi `stop_grace_period: 30s` dan bot yang hanya menangani
/// salah satunya akan dimatikan paksa oleh yang lain di tengah pengiriman.
async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("handler SIGINT bisa dipasang");
    };

    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut signal) => {
                signal.recv().await;
            }
            Err(error) => {
                // Tidak mungkin terjadi di Linux, tempat binary ini berjalan.
                // Dilaporkan alih-alih panik supaya port ke platform lain
                // turun jadi Ctrl-C saja, bukan menolak jalan.
                tracing::warn!(%error, "tidak ada handler SIGTERM; hanya Ctrl-C yang menghentikan");
                std::future::pending::<()>().await;
            }
        }
    };

    tokio::select! {
        () = ctrl_c => tracing::debug!("diinterupsi"),
        () = terminate => tracing::debug!("dihentikan"),
    }
}
