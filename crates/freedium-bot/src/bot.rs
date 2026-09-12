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
//! # Dua jalur masuk, satu pipeline
//!
//! Sebuah pesan bisa datang dari obrolan dengan bot, atau dari **panggilan
//! tamu** — seseorang menulis `@bot` di obrolan lain. Yang membedakan keduanya
//! cuma cara membalasnya: [`update::Source::Direct`] dibalas
//! `sendRichMessage` ke sebuah `chat_id`, [`update::Source::Guest`] dibalas
//! `answerGuestQuery` dengan `guest_query_id` dan tidak punya `chat_id` sama
//! sekali.
//!
//! Mengambil artikelnya identik di kedua jalur, jadi [`Bot::first_article`]
//! dipakai bersama. Yang **tidak** dibagi adalah penanganan galat, dan itu
//! bukan kelalaian: jalur tamu tidak bisa mengirim pesan galat ke obrolan orang,
//! jadi satu-satunya tempat untuk mengatakannya adalah hasil inline itu sendiri
//! — lihat [`rich::one_paragraph`].
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

/// Balasan ketika tautannya ada tapi tidak menuju artikel Medium.
///
/// Dipakai di kedua jalur — di DM sebagai pesan biasa, di panggilan tamu sebagai
/// satu-satunya isi hasilnya.
const NO_ARTICLE: &str = "Tidak menemukan artikel Medium di tautan itu. \
                          Kirim tautan langsung ke artikelnya, atau id postnya.";

/// Judul hasil inline untuk pesan yang **bukan** artikel.
///
/// `title` wajib ada di setiap hasil bertipe `article` — lihat
/// [`telegram::guest_query_result`] — termasuk pada hasil yang isinya cuma
/// kalimat galat. Isinya tidak pernah terlihat penerima; yang muncul adalah
/// paragraf di dalam pesannya.
const NOTICE_TITLE: &str = "Freedium";

/// `id` hasil inline untuk pesan yang bukan artikel.
///
/// Telegram mewajibkan setiap hasil punya `id`, dan yang ini tidak menandai
/// sebuah post — tidak ada post yang bisa ditunjuknya.
const NOTICE_ID: &str = "tamu";

/// Jatah foto yang dicoba berturut-turut untuk sebuah panggilan tamu.
///
/// Yang pertama tanpa batas: kalau Telegram menerimanya, tidak ada yang
/// dikorbankan, dan itulah yang terjadi pada hampir semua artikel. Yang kedua
/// menyisakan satu foto — gambar pratinjau di atas judul, yang membuat hasilnya
/// masih terlihat seperti artikel. Yang terakhir membuang fotonya sama sekali,
/// dan itu satu-satunya isi yang **diketahui** diterima: artikel satu foto
/// (`a61157615501`) lolos, artikel tujuh foto (`9d0b88a1763b`) ditolak, dengan
/// isi yang sama persis yang terkirim utuh di DM.
///
/// Urutannya menurun dan berhenti di yang pertama berhasil, jadi artikel yang
/// tidak bermasalah tidak pernah membayar satu pun percobaan tambahan.
const GUEST_PHOTO_LADDER: [usize; 3] = [usize::MAX, 1, 0];

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
            api: Arc::new(
                api::Client::new(
                    api_transport,
                    config.base_url.clone(),
                    config.request_timeout,
                )
                .with_api_token(config.api_token.clone()),
            ),
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
            // Boolean, bukan nilainya. Pertanyaan "kenapa aku kena 429" cuma
            // bisa dijawab kalau tier yang dipakai terlihat di log, dan tokennya
            // sendiri tidak pernah berguna di sana.
            trusted = config.api_token.is_some(),
            // `false` di sini bukan kegagalan boot: bot tetap bekerja di DM,
            // dan yang hilang cuma jalur tamunya. Yang penting adalah ia
            // terlihat, karena tidak ada galat yang akan menyebutkannya.
            guest = me.supports_guest_queries,
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

                let Some(message) = update.into_incoming() else {
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
        match message.source() {
            // Obrolan kita sendiri: balasannya pesan biasa, dan pesan galat pun
            // boleh dikirim ke sana.
            update::Source::Direct => self.handle_direct(message).await,
            update::Source::Guest(query_id) => self.handle_guest(message, query_id).await,
        }
    }

    /// Pesan di obrolan bot — DM, atau grup yang menyebutnya.
    async fn handle_direct(&self, message: &Message) {
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

        let (post_id, post) = match self.first_article(&candidates).await {
            Ok(found) => found,
            Err(NotFound::NoCandidate) => {
                self.reply(chat_id, NO_ARTICLE).await;
                return;
            }
            Err(NotFound::Api(error)) => {
                tracing::warn!("gagal mengambil artikel: {error}");
                self.reply(chat_id, &format!("Gagal mengambil artikelnya: {error}"))
                    .await;
                return;
            }
        };

        let rendered = self.render(&post_id, &post);

        self.deliver(chat_id, &post_id, &rendered).await;
    }

    /// Panggilan tamu — bot disebut di obrolan yang bukan miliknya.
    ///
    /// # Kenapa tidak ada balasan ketika pesannya tidak memuat tautan
    ///
    /// Karena memang tidak ada tempat untuk meletakkannya. Panggilan tamu tanpa
    /// hasil berarti tidak ada pesan yang muncul, dan itu juga persis yang
    /// terjadi kalau bot diam — jadi menambahkan "kirim tautan Medium" ke sini
    /// hanya akan membuat bot mengomentari obrolan orang setiap kali namanya
    /// disebut, yang justru alasan ia tidak membalas pesan tanpa tautan di DM.
    ///
    /// Yang **tidak** boleh diam adalah kegagalan setelah tautannya ada:
    /// pengguna sudah meminta sesuatu, dan tidak ada apa pun di layarnya yang
    /// memberi tahu bahwa permintaannya gagal.
    async fn handle_guest(&self, message: &Message, query_id: &str) {
        let candidates = update::url_candidates(message);

        if candidates.is_empty() {
            tracing::debug!("panggilan tamu tanpa tautan; tidak ada yang dijawab");
            return;
        }

        let (post_id, post) = match self.first_article(&candidates).await {
            Ok(found) => found,
            Err(NotFound::NoCandidate) => {
                self.guest_notice(query_id, NO_ARTICLE).await;
                return;
            }
            Err(NotFound::Api(error)) => {
                tracing::warn!("gagal mengambil artikel: {error}");
                self.guest_notice(query_id, &format!("Gagal mengambil artikelnya: {error}"))
                    .await;
                return;
            }
        };

        self.answer_guest_reduced(query_id, &post_id, &post).await
    }

    /// Menjawab panggilan tamu, menurunkan jatah fotonya sampai Telegram terima.
    ///
    /// # Kenapa harus bertahap, bukan sekali tebak
    ///
    /// Hasil inline menolak artikel bergambar dengan *"invalid inline message
    /// content specified"*, sementara isi yang sama persis terkirim utuh di DM.
    /// Yang mana yang sebenarnya jadi pembatas belum bisa dipastikan dari sini —
    /// jumlah medianya, satu media tertentu, atau sesuatu yang belum terpikir.
    ///
    /// Karena itu bot ini yang mencarinya, bukan sebuah angka yang ditebak di
    /// kode: ia menurunkan jatahnya sampai ada yang diterima, dan mencatat di
    /// mana berhentinya. Satu hari pemakaian sungguhan menjawab pertanyaan yang
    /// tidak bisa dijawab oleh pembacaan dokumen.
    ///
    /// Yang **tidak** diulang: penolakan karena sebab lain. Mengurangi foto
    /// tidak akan memperbaiki `guest_query_id` yang basi atau token yang salah,
    /// dan mengulanginya cuma menambah dua permintaan gagal ke log.
    async fn answer_guest_reduced(&self, query_id: &str, post_id: &str, post: &PostDto) {
        for (nomor, max_photos) in GUEST_PHOTO_LADDER.iter().enumerate() {
            let rendered = self.render_with_photos(post_id, post, *max_photos);

            match self
                .answer_guest(query_id, post_id, &post.meta.title, rendered)
                .await
            {
                Answered::Delivered | Answered::Failed => return,
                Answered::ContentRejected => {
                    if nomor + 1 < GUEST_PHOTO_LADDER.len() {
                        tracing::warn!(
                            post_id,
                            max_photos,
                            "isi inline ditolak; mencoba lagi dengan lebih sedikit foto"
                        );
                    }
                }
            }
        }
    }

    /// Menjawab panggilan tamu dengan satu paragraf pemberitahuan.
    ///
    /// Hasilnya dibuang dengan sengaja: pemberitahuan tidak punya foto yang
    /// bisa dikurangi, jadi tidak ada percobaan kedua yang masuk akal, dan
    /// [`Bot::answer_guest`] sudah mencatat kegagalannya sendiri.
    async fn guest_notice(&self, query_id: &str, text: &str) {
        let _ = self
            .answer_guest(query_id, NOTICE_ID, NOTICE_TITLE, rich::one_paragraph(text))
            .await;
    }

    /// Artikel jadi pesan, dengan pencatatan pemotongan yang sama di kedua jalur.
    fn render(&self, post_id: &str, post: &PostDto) -> rich::Rendered {
        self.render_with_photos(post_id, post, usize::MAX)
    }

    /// [`Bot::render`] dengan jatah foto — lihat [`GUEST_PHOTO_LADDER`].
    fn render_with_photos(
        &self,
        post_id: &str,
        post: &PostDto,
        max_photos: usize,
    ) -> rich::Rendered {
        let rendered = rich::rich_message_with_photos(post, self.api.base_url(), max_photos);

        if rendered.truncated {
            tracing::info!(post_id, "artikel dipotong supaya muat");
        }

        rendered
    }

    /// Mengirim satu hasil untuk sebuah panggilan tamu.
    ///
    /// Sekali coba, lalu sekali lagi kalau Telegram meminta menunggu — aturan
    /// yang sama dengan [`Bot::deliver`], dan sengaja tidak disatukan dengan
    /// helper generik: satu-satunya yang dibagi keduanya adalah bentuk loop-nya,
    /// sedangkan yang dikirim dan cara mencatatnya berbeda.
    ///
    /// Yang dikembalikan membedakan **kenapa** gagal, karena pemanggilnya
    /// memperlakukan ketiganya berbeda — lihat [`Answered`].
    async fn answer_guest(
        &self,
        query_id: &str,
        post_id: &str,
        title: &str,
        rendered: rich::Rendered,
    ) -> Answered {
        let result = telegram::guest_query_result(post_id, title, &rendered.message);
        let mut attempt = 0;

        loop {
            match self.telegram.answer_guest_query(query_id, &result).await {
                Ok(()) => {
                    tracing::info!(
                        post_id,
                        blocks = rendered.message.blocks.len(),
                        photos = rich::budget::Cost::of_blocks(&rendered.message.blocks).media,
                        truncated = rendered.truncated,
                        "artikel terjawab sebagai panggilan tamu"
                    );
                    return Answered::Delivered;
                }
                Err(error) => {
                    if let Some(wait) = error.retry_after()
                        && attempt == 0
                    {
                        attempt += 1;
                        tracing::warn!("dibatasi Telegram; menunggu {wait:?}");
                        tokio::time::sleep(wait).await;
                        continue;
                    }

                    // Tidak ada `reply`: `guest_query_id` bukan sebuah obrolan,
                    // dan mengirim pesan ke `chat_id` di sini berarti menambah
                    // satu pesan yang tidak diminta siapa pun ke obrolan orang.
                    tracing::warn!(post_id, "gagal menjawab panggilan tamu: {error}");
                    if let Some(advice) = error.advice() {
                        tracing::error!("{advice}");
                    }

                    return if error.is_inline_content_rejection() {
                        Answered::ContentRejected
                    } else {
                        Answered::Failed
                    };
                }
            }
        }
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
    ///
    /// # Kenapa keberhasilannya dicatat
    ///
    /// Sampai suatu saat, satu-satunya jejak sebuah artikel terkirim adalah
    /// ketiadaan galat. Itu membuat percobaan pertama tidak bisa dibedakan dari
    /// bot yang tidak pernah menerima pesannya — dua keadaan yang sangat berbeda,
    /// dan yang satu membuktikan seluruh jalurnya. Satu baris per artikel adalah
    /// harga yang pantas untuk bisa menjawab "apakah tadi terkirim".
    async fn deliver(&self, chat_id: i64, post_id: &str, rendered: &rich::Rendered) {
        let mut attempt = 0;

        loop {
            match self
                .telegram
                .send_rich_message(chat_id, &rendered.message)
                .await
            {
                Ok(()) => {
                    tracing::info!(
                        post_id,
                        chat_id,
                        blocks = rendered.message.blocks.len(),
                        truncated = rendered.truncated,
                        "artikel terkirim"
                    );
                    return;
                }
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

/// Apa yang terjadi pada satu percobaan menjawab panggilan tamu.
///
/// Tiga keadaan, bukan `bool`, karena pemanggilnya memperlakukan ketiganya
/// berbeda: yang pertama berhenti, yang kedua menurunkan jatah foto, yang
/// ketiga berhenti juga — dan menggabungkan dua yang terakhir jadi "gagal"
/// berarti membuang foto tanpa alasan pada setiap galat jaringan.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Answered {
    /// Telegram menerimanya.
    Delivered,
    /// Telegram menolak isinya. Jatah foto yang lebih kecil masih masuk akal
    /// dicoba — lihat [`GUEST_PHOTO_LADDER`].
    ContentRejected,
    /// Galat lain. Mengubah isi tidak akan menolongnya.
    Failed,
}
