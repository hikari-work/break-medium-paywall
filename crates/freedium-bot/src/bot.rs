//! Loop long polling: dari pembaruan Telegram jadi artikel terkirim.
//!
//! # Long polling, bukan webhook
//!
//! Compose stack-nya tidak punya jalur masuk sama sekali — kontainer hanya
//! loopback, nginx di host yang menghadap keluar. Webhook berarti menambah path
//! di vhost, mengelola secret token, dan memberi Telegram sebuah jalur masuk;
//! polling tidak butuh satu pun dari itu, dan trafiknya satu digit pesan per
//! menit. Yang dibayar: satu proses yang harus hidup terus, dan itu memang
//! bentuknya.
//!
//! # Satu token, satu pemegang offset
//!
//! `getUpdates` dengan token yang sama dari dua tempat menghasilkan `409
//! Conflict`, dan yang kalah akan terus kalah. Aturan ini sudah pernah menggigit
//! di repo ini, dan karena itu token bot artikel dipisah dari token notifier —
//! lihat [`crate::config::Config::telegram_token`].
//!
//! # Kesalahan yang terlihat, bukan yang disembunyikan
//!
//! Tiga hal yang sengaja **tidak** dilakukan:
//!
//! - **Tidak ada fallback ke `sendMessage` ber-markup saat rich message
//!   ditolak.** Yang dikirim sebagai gantinya adalah tautan ke halaman web dan
//!   satu baris yang mengatakan apa yang terjadi. Menebak markup berarti
//!   mengirim artikel yang berbeda dari halaman, dan itu justru hal yang bot ini
//!   ada untuk menghindari. Deskripsi penolakan Telegram ditulis ke log apa
//!   adanya — itu satu-satunya cara memperbaiki bentuknya.
//! - **Tidak ada pengulangan tanpa batas.** Setiap galat yang bukan pembatasan
//!   laju menghasilkan satu percobaan ulang, lalu satu pesan kepada pengguna.
//!   Pengulangan yang tak berujung mengubah satu artikel rusak menjadi beban.
//! - **Tidak ada penanda "sedang memproses".** Balasan yang datang sendiri sudah
//!   cukup; mengetik lalu mengedit berarti dua panggilan API untuk satu pesan,
//!   dan Telegram membatasi laju per obrolan.

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::Semaphore;

use medium_client::http::{ReqwestTransport, Transport};

use crate::api::{self, ApiError};
use crate::config::{Config, DEFAULT_CONCURRENCY};
use crate::rich;
use crate::telegram::{self, TelegramError};
use crate::update::{self, Message};
use freedium_dto::PostDto;

/// Jeda pertama setelah poll gagal. Digandakan setiap kegagalan berikutnya.
const BACKOFF_MIN: Duration = Duration::from_secs(1);

/// Batas jeda setelah poll gagal.
///
/// Dua menit, bukan tiga puluh detik: kalau Telegram atau jaringan sedang
/// bermasalah, satu percobaan tiap dua menit sudah lebih dari cukup untuk
/// kembali begitu keadaannya membaik, dan tidak cukup sering untuk memenuhi log.
const BACKOFF_MAX: Duration = Duration::from_secs(120);

/// Balasan ketika pengirimannya sendiri gagal, dan tidak ada yang lain yang
/// masih bisa dilakukan.
const SEND_FAILED: &str = "Gagal mengirim artikelnya. Coba lagi sebentar lagi.";

/// Balasan untuk `/start`, `/help`, dan pesan yang tidak memuat tautan.
const HELP: &str = "\
Kirim tautan artikel Medium ke sini, dan saya balas dengan isinya.

Contoh:
https://medium.com/@michalmalewicz/gpt-6-astra-just-ended-software-e6047997b667

Kalau artikelnya terlalu panjang untuk Telegram, saya kirim sebagian beserta \
tautan ke sisanya.";

/// Kenapa bot tidak bisa dijalankan.
#[derive(Debug, thiserror::Error)]
pub enum RunError {
    /// `TELEGRAM_ARTICLE_BOT_TOKEN` tidak diisi.
    ///
    /// Satu-satunya tempat ketiadaan token jadi kegagalan. `render` dan
    /// `healthcheck` tetap bisa dipakai tanpa itu — dan `render` justru alat
    /// verifikasi utama, jadi menolak jalan sebelum ada token berarti alat itu
    /// tidak bisa dipakai.
    #[error(
        "TELEGRAM_ARTICLE_BOT_TOKEN belum diisi. Token ini terpisah dari \
         TELEGRAM_BOT_TOKEN milik notifier alert, dan sengaja begitu: \
         menyalakan bot ini tidak boleh menyalakan kembali alert admin."
    )]
    MissingToken,
}

/// Bot, siap dijalankan.
///
/// Generik atas dua [`Transport`]: satu untuk API sendiri, satu untuk Telegram.
/// Dipisah karena keduanya punya kebijakan yang berbeda — tetapi pemanggilnya
/// hampir selalu mengisi keduanya dengan yang sama.
pub struct Bot<A: Transport, T: Transport> {
    api: Arc<api::Client<A>>,
    telegram: Arc<telegram::Client<T>>,
    concurrency: usize,
}

/// Ditulis tangan, bukan `#[derive(Clone)]`.
///
/// `derive` menambahkan batas `A: Clone` dan `T: Clone`, padahal yang disalin
/// cuma [`Arc`]-nya — dan transport yang tidak `Clone` seharusnya tetap bisa
/// dipakai. Yang lebih halus: tanpa `impl` ini, `self.clone()` di [`Bot::run`]
/// tidak menemukan `Bot::clone` sama sekali (batasnya tidak terpenuhi), lalu
/// jatuh ke `<&Bot as Clone>::clone` dan mengembalikan **referensi** ke `self` —
/// yang tidak bisa masuk `tokio::spawn`, dengan galat yang menunjuk ke tempat
/// yang sama sekali berbeda.
impl<A: Transport, T: Transport> Clone for Bot<A, T> {
    fn clone(&self) -> Self {
        Self {
            api: Arc::clone(&self.api),
            telegram: Arc::clone(&self.telegram),
            concurrency: self.concurrency,
        }
    }
}

impl Bot<ReqwestTransport, ReqwestTransport> {
    /// Bot yang benar-benar dijalankan.
    ///
    /// Gagal kalau tokennya tidak ada. Transportnya dibangun di sini supaya
    /// kesalahan konfigurasi TLS muncul saat boot, bukan pada pesan pertama.
    pub fn new(config: &Config) -> Result<Self, RunError> {
        let token = config
            .telegram_token
            .clone()
            .ok_or(RunError::MissingToken)?;

        Ok(Self::with_transports(
            ReqwestTransport::new().expect("klien HTTPS langsung selalu bisa dibangun"),
            ReqwestTransport::new().expect("klien HTTPS langsung selalu bisa dibangun"),
            &token,
            config,
        ))
    }
}

/// `'static` karena [`Bot::run`] men-`spawn` satu task per pesan. Batasnya
/// ditulis di sini alih-alih di definisi struct: [`Bot`] sendiri tidak butuh
/// itu, dan batas yang tidak perlu adalah batas yang menghalangi pemakaian yang
/// sah.
impl<A: Transport + 'static, T: Transport + 'static> Bot<A, T> {
    /// Bot dari transport yang sudah jadi — jalur yang dipakai test.
    #[must_use]
    pub fn with_transports(
        api_transport: A,
        telegram_transport: T,
        token: &str,
        config: &Config,
    ) -> Self {
        Self {
            api: Arc::new(api::Client::new(
                api_transport,
                config.base_url.clone(),
                config.request_timeout,
            )),
            telegram: Arc::new(telegram::Client::new(telegram_transport, token)),
            concurrency: DEFAULT_CONCURRENCY,
        }
    }

    /// Berapa pesan yang diproses bersamaan.
    #[must_use]
    pub fn with_concurrency(mut self, concurrency: usize) -> Self {
        self.concurrency = concurrency.max(1);
        self
    }

    /// Berjalan sampai `shutdown` selesai.
    ///
    /// `shutdown` diselesaikan dari luar — `main` memberinya SIGTERM dan SIGINT —
    /// supaya loop ini tidak perlu tahu apa-apa soal sinyal, dan supaya sebuah
    /// test bisa menghentikannya dengan menjatuhkan sebuah kanal.
    pub async fn run(
        &self,
        config: &Config,
        shutdown: impl std::future::Future<Output = ()>,
    ) -> Result<(), TelegramError> {
        let me = self.telegram.get_me().await?;
        tracing::info!(
            id = me.id,
            username = me.username.as_deref().unwrap_or("(tanpa nama pengguna)"),
            base_url = config.base_url,
            "bot siap"
        );

        let permits = Arc::new(Semaphore::new(self.concurrency));
        let mut offset = 0_i64;
        let mut backoff = BACKOFF_MIN;

        tokio::pin!(shutdown);

        loop {
            let poll = tokio::select! {
                () = &mut shutdown => {
                    tracing::info!("berhenti atas permintaan");
                    return Ok(());
                }
                result = self.telegram.get_updates(offset, config.poll_timeout) => result,
            };

            let updates = match poll {
                Ok(updates) => {
                    backoff = BACKOFF_MIN;
                    updates
                }
                Err(error) => {
                    // Satu token hanya boleh punya satu pemegang offset, jadi
                    // 409 tidak akan sembuh dengan menunggu lebih lama.
                    if let Some(advice) = error.advice() {
                        tracing::error!("{error}; {advice}");
                    } else {
                        tracing::warn!("getUpdates gagal: {error}");
                    }

                    let wait = error.retry_after().unwrap_or(backoff);
                    backoff = (backoff * 2).min(BACKOFF_MAX);

                    tokio::select! {
                        () = &mut shutdown => return Ok(()),
                        () = tokio::time::sleep(wait) => continue,
                    }
                }
            };

            for update in updates {
                // Offset dinaikkan **sebelum** pesannya diproses, bukan
                // sesudahnya: kalau prosesnya mati di tengah, pembaruan yang
                // sama tidak diulang dari awal pada boot berikutnya. Telegram
                // menyimpan yang belum diambil, jadi yang hilang cuma pesan yang
                // sedang diproses saat mati — dan itu pilihan yang benar
                // dibandingkan mengirim artikel yang sama dua kali ke pengguna
                // yang sama.
                offset = update.update_id + 1;

                let Some(message) = update.message else {
                    continue;
                };

                let Ok(permit) = permits.clone().acquire_owned().await else {
                    // Semaphore hanya ditutup saat shutdown, dan itu sudah
                    // ditangani di atas.
                    return Ok(());
                };

                let bot = self.clone();
                tokio::spawn(async move {
                    bot.handle(&message).await;
                    drop(permit);
                });
            }
        }
    }

    /// Satu pesan, dari kandidat sampai terkirim.
    ///
    /// Tidak pernah mengembalikan galat: pemanggilnya adalah sebuah task yang
    /// tidak punya siapa-siapa untuk melaporkannya, dan setiap kegagalan sudah
    /// jadi log atau pesan kepada pengguna di sini.
    async fn handle(&self, message: &Message) {
        let chat_id = message.chat.id;
        let candidates = update::url_candidates(message);

        if candidates.is_empty() {
            // Hanya perintah yang dibalas. Membalas setiap pesan yang tidak
            // memuat tautan berarti bot mengomentari percakapan yang bukan
            // urusannya — dan di grup, itu cara tercepat dikeluarkan.
            if matches!(
                update::command(message.text.as_deref().unwrap_or("")),
                Some("start" | "help")
            ) {
                self.reply(chat_id, HELP).await;
            }
            return;
        }

        let page_url = match self.first_article(&candidates).await {
            Ok(found) => found,
            Err(NotFound::NoCandidate) => {
                self.reply(
                    chat_id,
                    "Tidak menemukan artikel Medium di tautan itu. Kirim tautan \
                     langsung ke artikelnya, atau id postnya.",
                )
                .await;
                return;
            }
            Err(NotFound::Api(error)) => {
                tracing::warn!("gagal mengambil artikel: {error}");
                self.reply(chat_id, &format!("Gagal mengambil artikelnya: {error}"))
                    .await;
                return;
            }
        };

        let (post_id, post) = page_url;
        let rendered = rich::rich_message(&post, self.api.base_url());

        if rendered.truncated {
            tracing::info!(post_id, "artikel dipotong supaya muat");
        }

        self.deliver(chat_id, &post_id, &rendered).await;
    }

    /// Artikel pertama yang berhasil diselesaikan dari sekumpulan kandidat.
    ///
    /// Berhenti di kandidat pertama yang berhasil, jadi satu pesan berisi tiga
    /// tautan tidak menghasilkan tiga permintaan. Kandidat yang gagal dilewati
    /// hanya kalau kegagalannya berarti "bukan artikel" — galat API yang nyata
    /// dihentikan, karena mencoba tautan berikutnya tidak akan memperbaikinya.
    async fn first_article(&self, candidates: &[String]) -> Result<(String, PostDto), NotFound> {
        for candidate in candidates {
            // `400`/`404` berarti kandidat ini memang bukan artikel Medium, dan
            // kandidat berikutnya masih masuk akal dicoba. Galat lain tidak.
            match self.api.resolve(candidate).await {
                Ok(post_id) => match self.api.post(&post_id).await {
                    Ok(post) => return Ok((post_id, post)),
                    Err(error) if error.is_not_found() => {}
                    Err(error) => return Err(NotFound::Api(error)),
                },
                Err(error) if error.is_not_found() => {}
                Err(error) => return Err(NotFound::Api(error)),
            }
        }

        Err(NotFound::NoCandidate)
    }

    /// Mengirim rich message, dengan satu percobaan ulang untuk pembatasan laju.
    async fn deliver(&self, chat_id: i64, post_id: &str, rendered: &rich::Rendered) {
        let mut attempt = 0;

        loop {
            match self
                .telegram
                .send_rich_message(chat_id, &rendered.message)
                .await
            {
                Ok(()) => return,
                Err(error) => {
                    if let Some(wait) = error.retry_after()
                        && attempt == 0
                    {
                        attempt += 1;
                        tracing::warn!("dibatasi Telegram; menunggu {wait:?}");
                        tokio::time::sleep(wait).await;
                        continue;
                    }

                    self.report_send_failure(chat_id, post_id, &error).await;
                    return;
                }
            }
        }
    }

    /// Memberi tahu pengguna bahwa pengirimannya gagal, dan kenapa.
    ///
    /// Yang dikirim adalah tautan ke halaman ini. Itu bukan pengganti render
    /// yang lebih buruk — itu satu-satunya hal yang masih berguna ketika
    /// pengirimannya gagal, dan ia tidak berpura-pura jadi artikel.
    async fn report_send_failure(&self, chat_id: i64, post_id: &str, error: &TelegramError) {
        if error.is_rich_message_rejection() {
            // Deskripsi Telegram ditulis apa adanya: inilah satu-satunya cara
            // bentuk yang salah bisa diperbaiki.
            tracing::error!(
                post_id,
                "Telegram menolak rich message ini, dan tidak ada render cadangan: {error}"
            );
            self.reply(
                chat_id,
                &format!(
                    "Telegram menolak bentuk artikel ini. Bacanya di sini: {}",
                    self.api.page_url(post_id)
                ),
            )
            .await;
            return;
        }

        tracing::warn!(post_id, "gagal mengirim: {error}");

        if let Some(advice) = error.advice() {
            tracing::error!("{advice}");
        }

        self.reply(chat_id, SEND_FAILED).await;
    }

    /// Balasan teks polos, dan kegagalannya cuma jadi log.
    ///
    /// Kalau mengirim pesan galat pun gagal, tidak ada yang bisa dilakukan lagi —
    /// memberi tahu pengguna tentang kegagalan mengirim pemberitahuan kegagalan
    /// bukan pilihan.
    async fn reply(&self, chat_id: i64, text: &str) {
        if let Err(error) = self.telegram.send_message(chat_id, text).await {
            tracing::warn!("gagal membalas: {error}");
        }
    }
}

/// Kenapa tidak ada artikel yang bisa diambil.
enum NotFound {
    /// Semua kandidat dijawab "bukan artikel Medium".
    NoCandidate,
    /// Ada yang benar-benar rusak.
    Api(ApiError),
}
