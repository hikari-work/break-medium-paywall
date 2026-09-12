//! Klien Bot API — `getMe`, `getUpdates`, `sendRichMessage`.
//!
//! # Badan permintaannya JSON, bukan form
//!
//! `notify.rs` di `freedium-web` mengirim form karena `sendMessage` cuma punya
//! field skalar. `sendRichMessage` membawa **pohon** — `rich_message.blocks[]`
//! yang bersarang — dan form hanya bisa membawanya sebagai string JSON di dalam
//! sebuah field, yang berarti menyusun JSON dua kali. Bot API menerima badan
//! JSON untuk setiap metode (`Content-Type: application/json`), jadi
//! [`crate::rich::model`] diserialisasi sekali saja. Bahwa bentuk JSON inilah
//! yang belum pernah diuji — bukan keputusannya — dicatat di bagian terakhir
//! berkas ini.
//!
//! # Lewat [`Transport`] yang sama
//!
//! Alasan yang sama dengan [`crate::api`]: `medium-client` tetap satu-satunya
//! crate di workspace ini yang menyentuh jaringan keluar, dan sebuah test bisa
//! memeriksa badan yang akan dikirim tanpa soket.
//!
//! # Yang tidak dilakukan: menyembunyikan galat
//!
//! Setiap panggilan mengembalikan [`TelegramError`], dan tiga di antaranya punya
//! arti yang berbeda bagi pemanggilnya:
//!
//! - **`retry_after`** — Telegram sedang membatasi laju. Menunggu lalu mengulang
//!   adalah jawaban yang benar, dan mengulang tanpa menunggu memperpanjang
//!   pembatasannya.
//! - **409 Conflict** — ada pemanggil `getUpdates` kedua dengan token yang sama.
//!   Mengulang tidak akan pernah berhasil; yang benar adalah berhenti berisik.
//!   Aturan ini sudah pernah menggigit di repo ini, lihat catatan
//!   `uploadrecord-migration`.
//! - **Penolakan rich message** — badan yang kita susun tidak diterima Telegram.
//!   Ini satu-satunya cara mengetahui bentuk yang salah, jadi deskripsinya
//!   diteruskan apa adanya alih-alih diringkas jadi "gagal mengirim". Bedanya
//!   dengan dua di atas: yang ini **tidak** punya penanda yang bisa dicocokkan —
//!   lihat [`TelegramError::is_rich_message_rejection`].
//!
//! # Satu bentuk permintaan yang belum terbukti
//!
//! `rich_message` dikirim sebagai **objek JSON bersarang** di dalam badan
//! permintaan:
//!
//! ```json
//! {"chat_id": 1, "rich_message": {"blocks": [...]}}
//! ```
//!
//! Yang terbukti dari sumber acuan hanyalah bahwa `rich_message` adalah **satu**
//! parameter tingkat atas, dan bahwa server acuannya melakukan `query->arg(
//! "rich_message")` lalu `json_decode` — yaitu ia menerima **teks JSON**, yang
//! cocok dengan pengiriman multipart/form-data gaya aiogram. Apakah
//! `api.telegram.org` juga menerima objek bersarang di dalam `application/json`
//! belum pernah diuji. Kalau ternyata tidak, perubahannya satu baris di
//! [`Client::send_rich_message`]: `"rich_message": message` menjadi
//! `"rich_message": serde_json::to_string(message)`.

use std::time::Duration;

use medium_client::error::TransportError;
use medium_client::http::{
    Method, ReqwestTransport, Transport, TransportRequest, TransportResponse,
};
use serde::Deserialize;
use thiserror::Error;

use crate::rich::model::InputRichMessage;
use crate::update::Update;

/// Selisih yang ditambahkan ke timeout long poll untuk mendapat batas HTTP-nya.
///
/// Tanpa ini, timeout transportnya sama dengan `timeout` yang diminta ke
/// Telegram dan setiap poll akan dibatalkan tepat ketika ia hampir menjawab —
/// bukan gagal sesekali, tapi gagal terus, dengan galat yang terlihat seperti
/// Telegram yang tidak menjawab.
const POLL_GRACE: Duration = Duration::from_secs(10);

/// Batas satu panggilan yang **bukan** long poll.
const CALL_TIMEOUT: Duration = Duration::from_secs(15);

/// Nama metode pengiriman rich message, dan satu-satunya tempat ia dipakai
/// ulang di luar [`Client::send_rich_message`] — lihat
/// [`TelegramError::is_rich_message_rejection`].
const SEND_RICH_MESSAGE: &str = "sendRichMessage";

/// Bot API versi berapa yang diharapkan klien ini. Bukan yang dikirim — Bot API
/// tidak punya parameter versi — melainkan yang diingatkan ketika `sendRichMessage`
/// menjawab "method not found", yang artinya ada yang menunjuk server lama.
const MIN_BOT_API: &str = "10.1";

/// Kenapa sebuah panggilan Bot API gagal.
#[derive(Debug, Error)]
pub enum TelegramError {
    #[error(transparent)]
    Transport(#[from] TransportError),

    /// Telegram menjawab, dan jawabannya `ok: false`.
    #[error("Telegram menolak ({code}): {description}")]
    Api {
        /// Metode yang dipanggil, tanpa token — lihat [`TelegramError::is_rich_message_rejection`]
        /// soal kenapa ia perlu disimpan.
        method: &'static str,
        code: u16,
        description: String,
        /// Isi `parameters.retry_after`, kalau ada.
        retry_after: Option<u64>,
    },

    /// Jawabannya `200` tapi bukan amplop Bot API.
    #[error("jawaban Telegram tidak bisa dibaca: {0}")]
    Malformed(String),
}

impl TelegramError {
    /// Apakah Telegram meminta kita menunggu sebelum mencoba lagi.
    #[must_use]
    pub fn retry_after(&self) -> Option<Duration> {
        match self {
            Self::Api {
                retry_after: Some(seconds),
                ..
            } => Some(Duration::from_secs(*seconds)),
            _ => None,
        }
    }

    /// Apakah ada pemanggil `getUpdates` kedua dengan token yang sama.
    ///
    /// Mengulang tidak akan pernah berhasil: satu token hanya boleh punya satu
    /// pemegang offset, dan yang kalah akan terus kalah sampai yang menang
    /// berhenti. Pemanggil harus berhenti berisik, bukan mencoba lagi.
    #[must_use]
    pub fn is_conflict(&self) -> bool {
        matches!(self, Self::Api { code: 409, .. })
    }

    /// Apakah ini penolakan atas rich message yang kita susun.
    ///
    /// # Kenapa `400` dan bukan pencocokan kalimat
    ///
    /// Versi pertama fungsi ini mencari penanda `RICH` di dalam deskripsi
    /// Telegram. Penanda itu **tidak ada**. Ia dicari di halaman `sendRichMessage`
    /// Bot API, di skema yang bisa dibaca mesin, di `Client.cpp` dan
    /// `WebPageBlock.cpp` TDLib — tidak satu pun memuatnya. Yang benar-benar
    /// dikirim Telegram untuk rich message yang tidak valid adalah kalimat biasa
    /// ber-`400`:
    ///
    /// - `Bad Request: Rich message must be non-empty`
    /// - `Bad Request: type "X" is unsupported`
    /// - `Bad Request: Invalid list item type specified`
    /// - `Bad Request: List must be non-empty`
    /// - `Bad Request: List must be either ordered or unordered`
    /// - `Bad Request: Block is not allowed`
    ///
    /// Tidak ada pola yang bisa diandalkan dari daftar itu selain kodenya, jadi
    /// yang dipakai adalah pasangan **`400` + metode `sendRichMessage`**. Pasangan
    /// itu perlu karena `400` sendirian terlalu luas: `getUpdates` dengan offset
    /// yang salah juga menjawab `400`, dan mencatatnya sebagai "bentuk artikel
    /// ditolak" akan menyesatkan orang yang sedang membaca log.
    ///
    /// Konsekuensinya jujur: penolakan karena **chat** (bot diblokir, chat tidak
    /// ditemukan) juga ber-`400` dan akan masuk kategori ini. Yang terjadi
    /// hanyalah nadanya salah di log — deskripsi aslinya tetap ikut tertulis,
    /// dan itu yang benar-benar dibaca orang saat mencari kesalahan.
    #[must_use]
    pub fn is_rich_message_rejection(&self) -> bool {
        matches!(
            self,
            Self::Api {
                method: SEND_RICH_MESSAGE,
                code: 400,
                ..
            }
        )
    }

    /// Apakah metodenya tidak dikenal — Bot API yang ditunjuk terlalu tua.
    #[must_use]
    pub fn is_unknown_method(&self) -> bool {
        match self {
            Self::Api {
                code: 404,
                description,
                ..
            } => description.contains("method not found"),
            _ => false,
        }
    }

    /// Pesan yang layak ditulis ke log saat gagal.
    ///
    /// `is_unknown_method` dan `is_conflict` ada supaya pemanggil tidak perlu
    /// memeriksa kodenya sendiri, dan pesan ini yang memberi tahu operator apa
    /// yang harus dilakukan. [`MIN_BOT_API`] disebut di sini karena inilah
    /// satu-satunya tempat ia berguna.
    #[must_use]
    pub fn advice(&self) -> Option<String> {
        if self.is_unknown_method() {
            return Some(format!(
                "sendRichMessage butuh Bot API {MIN_BOT_API}; server yang menjawab ini lebih tua"
            ));
        }

        if self.is_conflict() {
            return Some(
                "token ini dipakai proses lain yang juga memanggil getUpdates; \
                 hentikan salah satunya — dua pemegang offset tidak bisa hidup bersama"
                    .to_string(),
            );
        }

        None
    }
}

/// Bot identitas kita, dari `getMe`.
#[derive(Debug, Clone, Deserialize)]
pub struct Me {
    pub id: i64,
    pub is_bot: bool,
    pub first_name: String,
    #[serde(default)]
    pub username: Option<String>,
}

/// Klien untuk satu token.
pub struct Client<T: Transport> {
    transport: T,
    token: String,
}

/// Klien yang benar-benar dijalankan bot.
pub type Http = Client<ReqwestTransport>;

impl<T: Transport> Client<T> {
    #[must_use]
    pub fn new(transport: T, token: impl Into<String>) -> Self {
        Self {
            transport,
            token: token.into(),
        }
    }

    /// Identitas bot. Ini yang dipanggil `healthcheck`.
    ///
    /// `getMe` dipilih sebagai denyut karena ia satu-satunya panggilan yang
    /// membuktikan **tokennya masih sah** — bukan hanya bahwa prosesnya hidup.
    /// Token yang dicabut lewat BotFather adalah kegagalan yang tidak terlihat
    /// dari dalam proses mana pun.
    pub async fn get_me(&self) -> Result<Me, TelegramError> {
        self.call::<Me>("getMe", &serde_json::json!({}), CALL_TIMEOUT)
            .await
    }

    /// Pembaruan berikutnya, menahan sampai `timeout` detik atau sampai ada
    /// yang datang.
    pub async fn get_updates(
        &self,
        offset: i64,
        timeout: Duration,
    ) -> Result<Vec<Update>, TelegramError> {
        // `allowed_updates` dibatasi ke `message` supaya bot tidak dibangunkan
        // oleh suntingan, poll, dan callback yang tidak diprosesnya — Telegram
        // berhenti mengirimkannya sama sekali, jadi tidak ada yang perlu
        // dibuang di sisi kita.
        let body = serde_json::json!({
            "offset": offset,
            "timeout": timeout.as_secs(),
            "allowed_updates": ["message"],
        });

        self.call("getUpdates", &body, timeout + POLL_GRACE).await
    }

    /// Mengirim artikel sebagai rich message.
    pub async fn send_rich_message(
        &self,
        chat_id: i64,
        message: &InputRichMessage,
    ) -> Result<(), TelegramError> {
        let body = serde_json::json!({
            "chat_id": chat_id,
            "rich_message": message,
        });

        self.call_ignoring_result(SEND_RICH_MESSAGE, &body, CALL_TIMEOUT)
            .await
    }

    /// Mengirim teks polos. Dipakai untuk pesan galat dan bantuan.
    ///
    /// Tanpa `parse_mode`: yang dikirim bot ini adalah kalimat yang disusunnya
    /// sendiri dan potongan deskripsi galat dari Telegram. Membiarkan keduanya
    /// ditafsirkan sebagai markup berarti satu tanda baca di deskripsi galat
    /// bisa membuat pengiriman galat itu sendiri gagal. Tanpa `link_preview_options`
    /// juga: pesan-pesan ini tidak memuat tautan yang perlu ditampilkan.
    pub async fn send_message(&self, chat_id: i64, text: &str) -> Result<(), TelegramError> {
        let body = serde_json::json!({
            "chat_id": chat_id,
            "text": text,
        });

        self.call_ignoring_result("sendMessage", &body, CALL_TIMEOUT)
            .await
    }

    /// Satu panggilan, hasilnya dibuang.
    async fn call_ignoring_result(
        &self,
        method: &'static str,
        body: &serde_json::Value,
        timeout: Duration,
    ) -> Result<(), TelegramError> {
        self.call::<serde_json::Value>(method, body, timeout)
            .await
            .map(|_| ())
    }

    /// Satu panggilan, lengkap dengan amplopnya.
    async fn call<R: serde::de::DeserializeOwned>(
        &self,
        method: &'static str,
        body: &serde_json::Value,
        timeout: Duration,
    ) -> Result<R, TelegramError> {
        let url = format!("https://api.telegram.org/bot{}/{method}", self.token);
        // Nama metode saja: URL-nya memuat token, dan token di dalam log adalah
        // token yang bocor ke siapa pun yang bisa membaca log.
        tracing::debug!(method, "memanggil Bot API");

        let response = self
            .transport
            .send(TransportRequest {
                url,
                method: Method::Post,
                headers: vec![("Content-Type".to_string(), "application/json".to_string())],
                body: serde_json::to_vec(body).expect("nilai JSON selalu bisa diserialisasi"),
                // `None`, selalu: Telegram bukan Medium, dan merutekan panggilan
                // bot lewat kumpulan exit WARP akan menghabiskan satu exit untuk
                // trafik yang tidak ada hubungannya — sama seperti `notify.rs`.
                proxy: None,
                timeout,
            })
            .await?;

        read(method, response)
    }
}

/// Amplop `{"ok":…,"result":…}` — atau `{"ok":false,…}`.
///
/// `result` sengaja `Value`, bukan parameter tipe: dengan parameter tipe, serde
/// menuntut `R: Deserialize<'de>` untuk setiap masa hidup, dan hasilnya adalah
/// batas yang tidak bisa dipenuhi [`serde::de::DeserializeOwned`] di dalam
/// fungsi generik. Dua langkah — amplop dulu, isinya kemudian — lebih jelas
/// sekaligus memberi tempat untuk pesan galat yang berbeda.
#[derive(Debug, Deserialize)]
struct Envelope {
    ok: bool,
    #[serde(default)]
    result: Option<serde_json::Value>,
    #[serde(default)]
    error_code: Option<u16>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    parameters: Option<Parameters>,
}

#[derive(Debug, Deserialize)]
struct Parameters {
    #[serde(default)]
    retry_after: Option<u64>,
}

/// Jawaban mentah jadi hasil, atau galat yang menjelaskan.
///
/// `status` HTTP-nya **tidak** dipakai untuk memutuskan berhasil atau tidak:
/// Telegram menjawab `ok: false` dengan status yang berbeda-beda (`400`, `429`,
/// `409`), dan ada perantara yang menjawab `200` untuk badan yang sebenarnya
/// galat. Yang menentukan adalah `ok`.
fn read<R: serde::de::DeserializeOwned>(
    method: &'static str,
    response: TransportResponse,
) -> Result<R, TelegramError> {
    let text = String::from_utf8_lossy(&response.body);
    let envelope: Envelope = serde_json::from_str(&text).map_err(|err| {
        // Badan yang tidak bisa dibaca hampir selalu berarti ada yang menjawab
        // selain Telegram: Cloudflare, proksi, atau halaman galat HTML.
        TelegramError::Malformed(format!(
            "{err} (HTTP {}, {} byte)",
            response.status,
            response.body.len()
        ))
    })?;

    if envelope.ok {
        // `result: true` untuk metode yang tidak mengembalikan apa-apa; ia
        // melewati `from_value::<Value>` tanpa keluhan.
        let result = envelope
            .result
            .ok_or_else(|| TelegramError::Malformed("ok: true tanpa result".to_string()))?;

        return serde_json::from_value(result).map_err(|err| {
            TelegramError::Malformed(format!("result tidak sesuai yang diharapkan: {err}"))
        });
    }

    Err(TelegramError::Api {
        method,
        code: envelope.error_code.unwrap_or(response.status),
        description: envelope
            .description
            .unwrap_or_else(|| "tanpa deskripsi".to_string()),
        retry_after: envelope.parameters.and_then(|it| it.retry_after),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rich::model::{InputRichBlock, InputRichMessage, RichText};
    use serde_json::{Value, json};
    use std::sync::{Arc, Mutex};

    /// Transport yang merekam permintaan dan menjawab dengan jawaban yang sudah
    /// disiapkan. Sama seperti di [`crate::api`], dan dengan alasan yang sama:
    /// yang diperiksa adalah badan yang benar-benar disusun.
    #[derive(Clone)]
    struct Recorder {
        sent: Arc<Mutex<Vec<TransportRequest>>>,
        answer: TransportResponse,
    }

    impl Recorder {
        fn answering(body: impl Into<Vec<u8>>) -> Self {
            Self {
                sent: Arc::new(Mutex::new(Vec::new())),
                answer: TransportResponse {
                    status: 200,
                    headers: Vec::new(),
                    body: body.into(),
                },
            }
        }

        fn sent(&self) -> Vec<TransportRequest> {
            self.sent.lock().expect("kunci tidak beracun").clone()
        }

        /// Badan yang dikirim, sebagai JSON — inilah yang diuji di hampir semua
        /// test di bawah.
        fn body(&self) -> Value {
            let sent = self.sent();
            serde_json::from_slice(&sent[0].body).expect("badannya JSON")
        }
    }

    #[async_trait::async_trait]
    impl Transport for Recorder {
        async fn send(
            &self,
            request: TransportRequest,
        ) -> Result<TransportResponse, TransportError> {
            self.sent.lock().expect("kunci tidak beracun").push(request);
            Ok(self.answer.clone())
        }
    }

    fn client(transport: Recorder) -> Client<Recorder> {
        Client::new(transport, "123:ABC")
    }

    fn sample_message() -> InputRichMessage {
        InputRichMessage::from_blocks(vec![InputRichBlock::Paragraph {
            text: RichText::plain("apa saja"),
        }])
    }

    // ---------------------------------------------------------------------
    // Bentuk permintaan
    // ---------------------------------------------------------------------

    #[test]
    fn get_me_posts_to_the_token_url() {
        let transport = Recorder::answering(
            r#"{"ok":true,"result":{"id":7,"is_bot":true,"first_name":"Freedium","username":"freedium_bot"}}"#,
        );

        let me = futures_lite_block_on(client(transport.clone()).get_me()).expect("ok");

        assert_eq!(me.id, 7);
        assert!(me.is_bot);
        assert_eq!(me.username.as_deref(), Some("freedium_bot"));

        let sent = transport.sent();
        assert_eq!(sent[0].url, "https://api.telegram.org/bot123:ABC/getMe");
        assert_eq!(sent[0].method, Method::Post);
        assert_eq!(sent[0].proxy, None, "selalu langsung: ini Telegram");
        assert!(
            sent[0]
                .headers
                .iter()
                .any(|(name, value)| { name == "Content-Type" && value == "application/json" })
        );
    }

    /// **Ini test terpenting di berkas ini.** `rich_message` harus jadi objek
    /// bersarang di dalam badan JSON, bukan string JSON di dalam sebuah field —
    /// itu bedanya badan JSON dari badan form, dan salah satunya menghasilkan
    /// penolakan validasi yang tidak menjelaskan apa-apa.
    #[test]
    fn a_rich_message_is_nested_json_not_a_stringified_field() {
        let transport = Recorder::answering(r#"{"ok":true,"result":{"message_id":1}}"#);

        futures_lite_block_on(client(transport.clone()).send_rich_message(42, &sample_message()))
            .expect("ok");

        let body = transport.body();
        assert_eq!(body["chat_id"], json!(42));
        assert_eq!(
            body["rich_message"]["blocks"][0],
            json!({ "type": "paragraph", "text": "apa saja" }),
            "bloknya harus jadi objek, bukan string: {body}"
        );
        assert_eq!(body["rich_message"]["skip_entity_detection"], json!(true));

        // Satu mode saja, selalu. `InputRichMessage` menerima tepat satu dari
        // `html`, `markdown`, `blocks`, dan mengirim dua berarti ditolak.
        assert!(body["rich_message"].get("html").is_none(), "{body}");
        assert!(body["rich_message"].get("markdown").is_none(), "{body}");
    }

    #[test]
    fn get_updates_sends_an_offset_and_asks_only_for_messages() {
        let transport = Recorder::answering(r#"{"ok":true,"result":[]}"#);

        let updates = futures_lite_block_on(
            client(transport.clone()).get_updates(81_234_568, Duration::from_secs(30)),
        )
        .expect("ok");

        assert!(updates.is_empty());

        let body = transport.body();
        assert_eq!(body["offset"], json!(81_234_568));
        assert_eq!(body["timeout"], json!(30));
        assert_eq!(
            body["allowed_updates"],
            json!(["message"]),
            "suntingan dan callback tidak diproses; jangan minta dikirimi"
        );
    }

    /// **Timeout HTTP long poll harus lebih panjang dari timeout poll-nya.**
    ///
    /// Kalau keduanya sama, setiap poll dibatalkan tepat ketika Telegram hampir
    /// menjawab — dan yang terlihat bukan "timeout terlalu pendek" melainkan
    /// "Telegram tidak pernah menjawab".
    #[test]
    fn the_http_timeout_outlives_the_long_poll() {
        let transport = Recorder::answering(r#"{"ok":true,"result":[]}"#);
        let poll = Duration::from_secs(30);

        futures_lite_block_on(client(transport.clone()).get_updates(0, poll)).expect("ok");

        let timeout = transport.sent()[0].timeout;
        assert!(
            timeout > poll,
            "{timeout:?} tidak lebih panjang dari {poll:?}"
        );
        assert_eq!(timeout, poll + POLL_GRACE);
    }

    /// Panggilan pendek tidak boleh ikut memakai timeout long poll: satu
    /// `sendRichMessage` yang menggantung selama 40 detik menahan antreannya.
    #[test]
    fn a_short_call_does_not_borrow_the_long_poll_timeout() {
        let transport = Recorder::answering(r#"{"ok":true,"result":true}"#);

        futures_lite_block_on(client(transport.clone()).send_message(42, "halo")).expect("ok");

        assert_eq!(transport.sent()[0].timeout, CALL_TIMEOUT);
        assert!(CALL_TIMEOUT < Duration::from_secs(30));
    }

    /// Tanpa `parse_mode`, teks bot dikirim apa adanya. Kalau tidak, satu tanda
    /// baca di deskripsi galat Telegram bisa menggagalkan pengiriman galat itu
    /// sendiri.
    #[test]
    fn a_plain_message_carries_no_parse_mode() {
        let transport = Recorder::answering(r#"{"ok":true,"result":{"message_id":1}}"#);

        futures_lite_block_on(client(transport.clone()).send_message(42, "halo <b>dunia</b>"))
            .expect("ok");

        let body = transport.body();
        assert_eq!(body["text"], json!("halo <b>dunia</b>"));
        assert!(body.get("parse_mode").is_none(), "{body}");
    }

    // ---------------------------------------------------------------------
    // Galat
    // ---------------------------------------------------------------------

    /// Galat dari `sendRichMessage`.
    ///
    /// [`TelegramError::is_rich_message_rejection`] memutuskan dari pasangan
    /// kode **dan** metode, jadi sebuah test tidak bisa menyusun galat rich
    /// message tanpa benar-benar melewati pengirimannya.
    fn rich_error(body: &str) -> TelegramError {
        let transport = Recorder::answering(body);
        futures_lite_block_on(client(transport).send_rich_message(1, &sample_message()))
            .expect_err("badannya ok: false")
    }

    fn error_for(body: &str) -> TelegramError {
        let transport = Recorder::answering(body);
        futures_lite_block_on(client(transport).get_me()).expect_err("bukan ok")
    }

    /// Menunggu lalu mengulang adalah jawaban yang benar untuk pembatasan laju;
    /// mengulang tanpa menunggu justru memperpanjangnya.
    #[test]
    fn a_rate_limit_carries_how_long_to_wait() {
        let error = error_for(
            r#"{"ok":false,"error_code":429,"description":"Too Many Requests: retry after 7","parameters":{"retry_after":7}}"#,
        );

        assert_eq!(error.retry_after(), Some(Duration::from_secs(7)));
        assert!(!error.is_conflict());
    }

    /// 409 berarti ada pemanggil `getUpdates` kedua dengan token yang sama.
    /// Mengulang tidak akan pernah berhasil, jadi pemanggilnya perlu tahu itu
    /// tanpa membaca deskripsinya sendiri.
    #[test]
    fn a_conflict_is_recognised_and_comes_with_advice() {
        let error = error_for(
            r#"{"ok":false,"error_code":409,"description":"Conflict: terminated by other getUpdates request"}"#,
        );

        assert!(error.is_conflict());
        assert_eq!(error.retry_after(), None, "menunggu tidak akan menolong");
        assert!(
            error
                .advice()
                .expect("ada nasihatnya")
                .contains("getUpdates"),
            "{error}"
        );
    }

    /// Penolakan rich message adalah satu-satunya cara mengetahui bentuk yang
    /// kita susun salah, jadi deskripsi aslinya harus sampai utuh.
    ///
    /// Kalimatnya diambil apa adanya dari yang benar-benar dikirim Telegram —
    /// lihat [`TelegramError::is_rich_message_rejection`] soal kenapa yang
    /// dicocokkan cuma kodenya.
    #[test]
    fn a_rich_message_rejection_keeps_telegrams_own_words() {
        let error = rich_error(
            r#"{"ok":false,"error_code":400,"description":"Bad Request: Invalid list item type specified"}"#,
        );

        assert!(error.is_rich_message_rejection());
        assert!(
            error
                .to_string()
                .contains("Invalid list item type specified"),
            "deskripsi Telegram tidak boleh diringkas: {error}"
        );
        assert!(error.advice().is_none(), "bukan masalah konfigurasi");
    }

    /// `400` sendirian terlalu luas: `getUpdates` dengan offset yang salah juga
    /// menjawab `400`, dan menyebutnya "bentuk artikel ditolak" akan menyesatkan
    /// orang yang membaca log.
    #[test]
    fn a_four_hundred_from_another_method_is_not_a_rich_message_rejection() {
        let error = error_for(
            r#"{"ok":false,"error_code":400,"description":"Bad Request: invalid offset"}"#,
        );

        assert!(!error.is_rich_message_rejection(), "{error:?}");
    }

    /// Dan sebaliknya: `sendRichMessage` yang gagal karena chatnya, bukan karena
    /// bentuknya, tetap masuk kategori ini. Itu disengaja dan didokumentasikan —
    /// deskripsi aslinya tetap ikut tertulis, jadi yang salah cuma nada di log.
    #[test]
    fn a_rich_message_rejection_needs_the_code_and_the_method_together() {
        for code in [403, 429] {
            let error = rich_error(&format!(
                r#"{{"ok":false,"error_code":{code},"description":"nope"}}"#
            ));

            assert!(!error.is_rich_message_rejection(), "kode {code}");
        }
    }

    /// Bot API yang ditunjuk terlalu tua. Pesannya harus menyebut versinya,
    /// karena itulah satu-satunya hal yang bisa dilakukan operator.
    #[test]
    fn an_unknown_method_names_the_bot_api_version_we_need() {
        let error = error_for(
            r#"{"ok":false,"error_code":404,"description":"Not Found: method not found"}"#,
        );

        assert!(error.is_unknown_method());
        let advice = error.advice().expect("ada nasihatnya");
        assert!(advice.contains(MIN_BOT_API), "{advice}");
        assert!(!error.is_rich_message_rejection());
    }

    /// Galat lain tidak diklaim sebagai salah satu dari ketiganya.
    #[test]
    fn an_ordinary_error_claims_nothing_special() {
        let error = error_for(
            r#"{"ok":false,"error_code":403,"description":"Forbidden: bot was blocked by the user"}"#,
        );

        assert!(!error.is_conflict());
        assert!(!error.is_rich_message_rejection());
        assert!(!error.is_unknown_method());
        assert_eq!(error.retry_after(), None);
        assert!(error.advice().is_none());
    }

    /// Ada perantara yang menjawab `200` untuk badan yang sebenarnya galat, dan
    /// ada yang menjawab `400` untuk badan yang sah. Yang menentukan adalah
    /// `ok`, bukan status HTTP-nya.
    #[test]
    fn ok_is_what_decides_not_the_http_status() {
        let mut transport =
            Recorder::answering(r#"{"ok":false,"error_code":400,"description":"nope"}"#);
        transport.answer.status = 200;

        let error = futures_lite_block_on(client(transport).get_me()).expect_err("ok: false");
        assert!(
            matches!(error, TelegramError::Api { code: 400, .. }),
            "{error:?}"
        );
    }

    #[test]
    fn an_error_without_an_error_code_falls_back_to_the_http_status() {
        let mut transport = Recorder::answering(r#"{"ok":false,"description":"tanpa kode"}"#);
        transport.answer.status = 502;

        let error = futures_lite_block_on(client(transport).get_me()).expect_err("ok: false");
        assert!(
            matches!(error, TelegramError::Api { code: 502, .. }),
            "{error:?}"
        );
    }

    /// Badan yang bukan amplop Bot API hampir selalu berarti ada perantara yang
    /// menjawab — dan ukurannya disebut supaya operator tahu apa yang datang.
    #[test]
    fn a_body_that_is_not_an_envelope_is_malformed() {
        let error = error_for("<html><body>502 Bad Gateway</body></html>");

        match error {
            TelegramError::Malformed(message) => {
                assert!(
                    message.contains("502") || message.contains("byte"),
                    "{message}"
                );
            }
            other => panic!("harusnya Malformed, bukan {other:?}"),
        }
    }

    /// `ok: true` tanpa `result` bukan keberhasilan yang bisa dipakai: pemanggil
    /// yang mengharapkan `Me` atau `Vec<Update>` akan menerima `None`.
    #[test]
    fn ok_true_without_a_result_is_malformed() {
        let error = error_for(r#"{"ok":true}"#);
        assert!(matches!(error, TelegramError::Malformed(_)), "{error:?}");
    }

    /// Transport yang gagal tetap gagal transport, bukan berubah jadi galat API:
    /// pemanggilnya perlu membedakan "Telegram bilang tidak" dari "tidak sampai
    /// ke Telegram".
    #[test]
    fn a_transport_failure_stays_a_transport_failure() {
        #[derive(Clone)]
        struct Broken;

        #[async_trait::async_trait]
        impl Transport for Broken {
            async fn send(
                &self,
                _request: TransportRequest,
            ) -> Result<TransportResponse, TransportError> {
                Err(TransportError::Timeout)
            }
        }

        let error = futures_lite_block_on(Client::new(Broken, "t").get_me())
            .expect_err("transportnya memang rusak");

        assert!(matches!(error, TelegramError::Transport(_)), "{error:?}");
    }

    /// `tokio` ada di dependensi biasa, tapi test di modul ini sinkron. Alih-alih
    /// menandai setiap test `#[tokio::test]` dan membayar runtime untuk pekerjaan
    /// yang tidak menunggu apa pun, satu pemblokir kecil dipakai bersama.
    fn futures_lite_block_on<F: std::future::Future>(future: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime bisa dibangun")
            .block_on(future)
    }
}
