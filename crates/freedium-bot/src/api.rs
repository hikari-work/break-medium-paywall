//! Klien `/api/v1` — satu-satunya jalan bot ini mendapatkan artikel.
//!
//! # Kenapa lewat HTTP, bukan IR di dalam proses
//!
//! Merender langsung dari `medium-doc` seperti `medium-render` akan menghindari
//! serialisasi dan tidak lossy. Yang harus dibayar: bot jadi menautkan
//! `freedium-cache` dan `medium-client`, dan keduanya **syarat boot** —
//! `AppState::new` melakukan `init_db` dan handshake Redis di awal. Bot yang
//! tugasnya mengubah URL jadi pesan akan mati kalau Redis mati. Lihat dokumen
//! [`crate`] untuk tabel lengkap apa yang hilang dan kenapa tidak ada yang
//! berarti.
//!
//! # Ia memakai [`Transport`] yang sama dengan sisa workspace
//!
//! `medium-client` adalah satu-satunya crate di workspace ini yang menyentuh
//! jaringan keluar, dan itu tetap benar di sini: bot mengirim permintaannya
//! lewat [`Transport`], bukan lewat klien HTTP kedua. Efek sampingnya yang
//! sebenarnya penting: sebuah test bisa memeriksa **apa yang akan dikirim**
//! tanpa soket, dan pertanyaan "apakah jalur lokal benar-benar melewati
//! jaringan" jadi bisa dijawab.
//!
//! # `X-API-TOKEN`, dan kenapa keputusan sebelumnya dibalik
//!
//! Sampai satu titik, klien ini **sengaja tidak pernah** mengirim header itu.
//! Alasannya waktu itu: `/api/v1` terbuka untuk pembacaan, token cuma menaikkan
//! jatah laju, dan menyimpan satu rahasia untuk membeli kuota yang tidak pernah
//! habis tidak sepadan.
//!
//! Yang membuat hitungannya berubah bukan kuota yang lebih besar, melainkan
//! **burst**-nya. Tier anonim punya `miss` 3 per menit dengan burst **1**
//! (`freedium-web/src/api/limit.rs`), dan satu artikel menghabiskan dua
//! permintaan miss — `resolve` lalu `posts`. Artinya orang yang mengirim dua
//! tautan baru dalam dua puluh detik kena `429` pada tautan kedua. Itu bukan
//! pembatasan yang masuk akal untuk satu orang yang mengirim tautan ke dirinya
//! sendiri, dan justru itulah pemakaian bot ini.
//!
//! Tier tepercaya tidak menaikkan anggaran fetch Medium sedikit pun — kuota
//! `fetch` proses-global tetap sama dan tetap dikenakan ke semua orang. Ia hanya
//! memindahkan biaya melayani satu klien dari "pemindai internet" ke "pemilik
//! server". Karena itu kuota anonim tetap seperti semula.
//!
//! # Tiga hal yang dijaga di sini
//!
//! - **`None` berarti header tidak dikirim**, bukan header kosong. Header kosong
//!   adalah klaim, dan server menjawabnya `401` — lihat `token_tier` di sana.
//!   Klien yang menurunkannya diam-diam ke tier anonim akan menyembunyikan token
//!   yang salah sampai hari jatahnya benar-benar habis.
//! - **Tokennya tidak pernah masuk log.** Satu-satunya tempat ia muncul adalah
//!   daftar header permintaan. Log `debug!` di [`Client::get`] mencetak URL-nya
//!   saja.
//! - **Nama variabelnya sama dengan milik server** (`API_TOKEN`), supaya satu
//!   rahasia tidak pernah punya dua nilai. Lihat [`crate::config::Config`].

use std::time::Duration;

use freedium_dto::problem::Problem;
use freedium_dto::{PostDto, ResolveDto};
use medium_client::error::TransportError;
use medium_client::http::{
    Method, ReqwestTransport, Transport, TransportRequest, TransportResponse,
};
use thiserror::Error;

/// Panjang id post Medium. Id yang panjangnya lain tetap bisa diselesaikan,
/// hanya saja lewat server — lihat [`bare_post_id`].
const POST_ID_LEN: usize = 12;

/// Protokol yang dikirim bot. Bukan hiasan: ia muncul di log Cloudflare, dan
/// satu-satunya cara membedakan trafik bot dari trafik peramban ketika ada yang
/// perlu dipertanggungjawabkan.
const USER_AGENT: &str = concat!("freedium-bot/", env!("CARGO_PKG_VERSION"));

/// Nama header tier tepercaya.
///
/// Ditulis di sini alih-alih diimpor dari `freedium-web`: bot ini tidak
/// menautkan crate itu — bot sengaja hanya bicara lewat HTTP — dan satu string
/// yang harus cocok adalah hal yang paling murah untuk diduplikasi. Yang
/// menjaganya tetap cocok bukan kompilator melainkan test di bawah, yang
/// memeriksa header yang benar-benar disusun.
const API_TOKEN_HEADER: &str = "X-API-TOKEN";

/// Kenapa sebuah panggilan API gagal.
#[derive(Debug, Error)]
pub enum ApiError {
    #[error(transparent)]
    Transport(#[from] TransportError),

    /// API-nya menjawab, tapi bukan `200`.
    ///
    /// `problem` terisi ketika jawabannya memang dokumen RFC 9457 — yang hampir
    /// selalu, karena itu satu-satunya bentuk galat `/api/v1`. `None` berarti
    /// ada yang menjawab selain API kita: Cloudflare, nginx, atau captive portal.
    /// Membedakan keduanya penting saat mencari kesalahan, dan pesannya berbeda.
    #[error("{}", describe(*status, problem.as_deref()))]
    Status {
        status: u16,
        problem: Option<Box<Problem>>,
    },

    /// Jawabannya `200` tapi bukan JSON yang kita kenal.
    #[error("jawaban /api/v1 tidak bisa dibaca: {0}")]
    Malformed(String),
}

fn describe(status: u16, problem: Option<&Problem>) -> String {
    match problem {
        Some(problem) => format!("{} (HTTP {status}): {}", problem.r#type, problem.detail),
        None => format!("HTTP {status} dari sesuatu yang bukan /api/v1"),
    }
}

impl ApiError {
    /// Apakah kegagalannya berarti "bukan artikel Medium", bukan "ada yang rusak".
    ///
    /// `400` dan `404` adalah jawaban `/api/v1` untuk input yang bukan URL
    /// Medium dan untuk id yang tidak ada. Keduanya berarti kandidat **ini** yang
    /// salah, dan kandidat berikutnya di pesan yang sama masih masuk akal dicoba.
    ///
    /// `429`, `5xx`, dan kegagalan transport tidak masuk kategori ini: mencoba
    /// tautan berikutnya tidak akan memperbaikinya, dan yang terjadi hanyalah
    /// jatah laju yang habis lebih cepat — jatah yang, kalau kosong, membuat
    /// situsnya berhenti melayani semua orang.
    #[must_use]
    pub fn is_not_found(&self) -> bool {
        matches!(
            self,
            Self::Status {
                status: 400 | 404,
                ..
            }
        )
    }
}

/// Klien untuk satu host.
///
/// Generik atas [`Transport`] supaya sebuah test bisa memeriksa permintaan yang
/// akan dikirim tanpa soket — alasan yang sama dengan `freedium-web/src/notify.rs`.
pub struct Client<T: Transport> {
    transport: T,
    base_url: String,
    timeout: Duration,
    /// `None` berarti header tidak dikirim sama sekali. Lihat catatan modul.
    api_token: Option<String>,
}

/// Klien yang benar-benar dijalankan bot.
pub type Http = Client<ReqwestTransport>;

impl<T: Transport> Client<T> {
    #[must_use]
    pub fn new(transport: T, base_url: impl Into<String>, timeout: Duration) -> Self {
        Self {
            transport,
            base_url: base_url.into().trim_end_matches('/').to_string(),
            timeout,
            api_token: None,
        }
    }

    /// Mengirim `X-API-TOKEN` pada setiap permintaan berikutnya.
    ///
    /// Dipisah dari [`Client::new`] karena kredensial ini opsional dan ortogonal:
    /// tiga argumen posisional sudah cukup banyak, dan menambahkan `None` di
    /// setiap pemanggil yang tidak peduli akan menyembunyikan bahwa ia memang
    /// opsional. Dipanggil sekali di [`crate::bot::Bot::with_transports`].
    ///
    /// `None` berarti "tidak ada token", sama seperti [`crate::config::Config`]:
    /// header tidak dikirim sama sekali, bukan dikirim kosong.
    #[must_use]
    pub fn with_api_token(mut self, token: Option<String>) -> Self {
        self.api_token = token;
        self
    }

    /// Header yang dikirim pada setiap permintaan.
    ///
    /// Satu tempat, supaya "apakah token ikut" bisa diperiksa sebagai nilai,
    /// bukan dengan membaca ulang badan [`Client::get`].
    fn headers(&self) -> Vec<(String, String)> {
        let mut headers = vec![("User-Agent".to_string(), USER_AGENT.to_string())];

        // Header kosong adalah klaim token yang salah, bukan "tanpa token" —
        // itu jawaban `401` dari server, bukan tier anonim. Karena itu yang
        // dikirim hanya token yang benar-benar ada.
        if let Some(token) = &self.api_token {
            headers.push((API_TOKEN_HEADER.to_string(), token.clone()));
        }

        headers
    }

    /// Halaman yang dirender bot ini, untuk artikel yang dipotong.
    ///
    /// Bentuknya `{base}/p/{id}` — bukan URL kanonik artikelnya
    /// (`MetaDto::medium_url`, yang butuh satu kali pengambilan lagi). Yang
    /// dibutuhkan pembaca yang kehabisan tempat adalah halaman **kita**, tempat
    /// sisa artikelnya sudah dirender.
    #[must_use]
    pub fn page_url(&self, post_id: &str) -> String {
        format!("{}/p/{post_id}", self.base_url)
    }

    /// Alamat deployment ini, tanpa garis miring di ujungnya.
    ///
    /// [`crate::rich::rich_message`] membutuhkannya sebagai basis tautan
    /// penutup, dan itu satu-satunya alasan ia terbuka: pemanggil tidak boleh
    /// menyusun URL sendiri, karena `page_url` dan `rich` harus menunjuk host
    /// yang sama persis.
    #[must_use]
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// Id di balik sebuah URL Medium.
    ///
    /// # Jalur lokal, dan kenapa ia sempit
    ///
    /// Kalau `input` **hanya** sebuah id — dua belas digit heksadesimal dan
    /// tidak ada apa-apa lagi — id itu dipakai langsung tanpa menyentuh
    /// jaringan. Selain itu, `input` dikirim apa adanya ke `/api/v1/resolve`.
    ///
    /// Aturannya sengaja sesempit itu. Mengambil id dari ekor sebuah URL
    /// (`…-e6047997b667`) tampak mudah, tapi itu berarti implementasi kedua dari
    /// "id mana yang dimaksud URL ini", yang bisa berbeda dari implementasi
    /// server. Bedanya tidak terlihat dan tidak berbahaya: bot akan mengirim
    /// artikel yang **lain** dari yang ditautkan pengguna. Satu id telanjang
    /// bukan URL sama sekali, jadi tidak ada aturan yang diduplikasi — dan
    /// `/api/v1/resolve` sendiri memang menerima id telanjang, jadi bentuknya
    /// sah di kedua jalur.
    pub async fn resolve(&self, input: &str) -> Result<String, ApiError> {
        let trimmed = input.trim();
        if let Some(post_id) = bare_post_id(trimmed) {
            tracing::debug!(post_id, "id telanjang; tidak perlu bertanya ke server");
            return Ok(post_id.to_string());
        }

        let query = form_urlencoded::Serializer::new(String::new())
            .append_pair("url", trimmed)
            .finish();

        let body = self.get(&format!("/api/v1/resolve?{query}")).await?;
        let resolved: ResolveDto = parse(&body)?;
        Ok(resolved.post_id)
    }

    /// Entitas sebuah post.
    pub async fn post(&self, post_id: &str) -> Result<PostDto, ApiError> {
        let body = self.get(&format!("/api/v1/posts/{post_id}")).await?;
        parse(&body)
    }

    async fn get(&self, path_and_query: &str) -> Result<Vec<u8>, ApiError> {
        let url = format!("{}{path_and_query}", self.base_url);
        tracing::debug!(url, "GET");

        let response = self
            .transport
            .send(TransportRequest {
                url,
                method: Method::Get,
                headers: self.headers(),
                body: Vec::new(),
                // `None`, selalu. Ini host kita sendiri; merutekannya lewat
                // kumpulan exit WARP akan menghabiskan satu exit untuk trafik
                // yang tidak ke Medium, dan bisa kehilangan jawabannya.
                proxy: None,
                timeout: self.timeout,
            })
            .await?;

        classify(response)
    }
}

/// Jawaban transport jadi isi badan, atau galat yang menjelaskan.
fn classify(response: TransportResponse) -> Result<Vec<u8>, ApiError> {
    if response.status == 200 {
        return Ok(response.body);
    }

    // Badan yang bukan UTF-8 bukan dokumen problem; yang dilaporkan tetap
    // statusnya, jadi tidak ada gunanya memperlakukan ini sebagai kegagalan
    // tersendiri.
    let text = String::from_utf8_lossy(&response.body);
    let problem = serde_json::from_str::<Problem>(&text).ok().map(Box::new);

    Err(ApiError::Status {
        status: response.status,
        problem,
    })
}

fn parse<T: serde::de::DeserializeOwned>(body: &[u8]) -> Result<T, ApiError> {
    serde_json::from_slice(body).map_err(|err| ApiError::Malformed(err.to_string()))
}

/// Sebuah id telanjang: dua belas digit heksadesimal, tidak kurang tidak lebih.
///
/// Panjangnya tetap dua belas karena itu yang diterbitkan Medium. Id yang
/// panjangnya lain — `medium-doc` menerima 8 sampai 12 — tidak salah dikenali;
/// ia hanya lewat server, yang toh bisa menyelesaikannya. Arah kesalahannya
/// satu kali perjalanan HTTP, bukan artikel yang salah.
#[must_use]
pub fn bare_post_id(input: &str) -> Option<&str> {
    (input.len() == POST_ID_LEN && input.bytes().all(|byte| byte.is_ascii_hexdigit()))
        .then_some(input)
}

#[cfg(test)]
mod tests {
    use super::*;
    use medium_client::http::TransportResponse;
    use std::sync::{Arc, Mutex};

    /// Transport yang mencatat apa yang diminta dan menjawab dengan jawaban
    /// yang sudah disiapkan.
    ///
    /// Rekaman, bukan mock: yang diperiksa adalah permintaan yang **benar-benar
    /// disusun**, dan itu satu-satunya cara membuktikan sebuah jalur tidak
    /// menyentuh jaringan sama sekali.
    #[derive(Clone)]
    struct Recorder {
        sent: Arc<Mutex<Vec<TransportRequest>>>,
        answer: TransportResponse,
    }

    impl Recorder {
        fn answering(status: u16, body: impl Into<Vec<u8>>) -> Self {
            Self {
                sent: Arc::new(Mutex::new(Vec::new())),
                answer: TransportResponse {
                    status,
                    headers: Vec::new(),
                    body: body.into(),
                },
            }
        }

        fn sent(&self) -> Vec<TransportRequest> {
            self.sent.lock().expect("kunci tidak beracun").clone()
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
        Client::new(transport, "https://contoh.test/", Duration::from_secs(5))
    }

    /// Header yang dikirim, apa adanya.
    fn headers(request: &TransportRequest) -> Vec<(String, String)> {
        request.headers.clone()
    }

    // ---------------------------------------------------------------------
    // Id telanjang
    // ---------------------------------------------------------------------

    #[test]
    fn a_bare_post_id_is_twelve_hex_digits_and_nothing_else() {
        assert_eq!(bare_post_id("e6047997b667"), Some("e6047997b667"));
        assert_eq!(bare_post_id("E6047997B667"), Some("E6047997B667"));

        for not_an_id in [
            "",
            "e6047997b66",
            "e6047997b6677",
            "e6047997b66g",
            "https://medium.com/p/e6047997b667",
            "e6047997b667 ",
            "not a post",
        ] {
            assert_eq!(bare_post_id(not_an_id), None, "{not_an_id:?}");
        }
    }

    /// Pertanyaan utamanya bukan "apakah hasilnya benar" melainkan "apakah
    /// jaringan disentuh" — satu id telanjang sudah cukup untuk menjawabnya.
    #[tokio::test]
    async fn a_bare_post_id_never_reaches_the_network() {
        let transport = Recorder::answering(500, "");
        let post_id = client(transport.clone())
            .resolve("  e6047997b667  ")
            .await
            .expect("id telanjang selalu berhasil");

        assert_eq!(post_id, "e6047997b667");
        assert!(
            transport.sent().is_empty(),
            "tidak ada permintaan yang seharusnya dikirim"
        );
    }

    /// **URL tidak pernah ditebak id-nya sendiri.**
    ///
    /// Test ini yang menjaga keputusan di [`Client::resolve`]: sepanjang URL
    /// dikirim ke server, berapa pun mudahnya id-nya dibaca dari ekornya.
    #[tokio::test]
    async fn a_medium_url_is_always_asked_about() {
        let transport = Recorder::answering(
            200,
            r#"{"schema_version":1,"post_id":"e6047997b667","resolved_url":"https://medium.com/p/e6047997b667"}"#,
        );

        let post_id = client(transport.clone())
            .resolve("https://medium.com/@x/judul-e6047997b667")
            .await
            .expect("server menjawab 200");

        assert_eq!(post_id, "e6047997b667");

        let sent = transport.sent();
        assert_eq!(sent.len(), 1);
        assert_eq!(
            sent[0].url,
            "https://contoh.test/api/v1/resolve?url=https%3A%2F%2Fmedium.com%2F%40x%2Fjudul-e6047997b667"
        );
        assert_eq!(sent[0].method, Method::Get);
        assert_eq!(sent[0].proxy, None, "selalu langsung: ini host sendiri");
        assert_eq!(sent[0].body, Vec::<u8>::new(), "GET tidak berbadan");
    }

    // ---------------------------------------------------------------------
    // Post
    // ---------------------------------------------------------------------

    #[tokio::test]
    async fn a_post_is_fetched_by_id() {
        let transport = Recorder::answering(
            200,
            include_str!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/fixtures/gpt-6-astra.json"
            )),
        );

        let post = client(transport.clone())
            .post("e6047997b667")
            .await
            .expect("jawaban sungguhan bisa dibaca");

        assert_eq!(post.meta.title, "GPT-6 Astra just ended software.");
        assert_eq!(post.blocks.len(), 107);
        assert_eq!(
            transport.sent()[0].url,
            "https://contoh.test/api/v1/posts/e6047997b667"
        );
    }

    // ---------------------------------------------------------------------
    // Galat
    // ---------------------------------------------------------------------

    #[tokio::test]
    async fn a_problem_document_becomes_the_error_message() {
        let transport = Recorder::answering(
            404,
            r#"{"schema_version":1,"type":"/problems/not-found","title":"Not Found","status":404,"detail":"no post with that id","instance":"/api/v1/posts/x","request_id":"a-b-c"}"#,
        );

        let error = client(transport)
            .post("e6047997b667")
            .await
            .expect_err("404 bukan 200");

        let message = error.to_string();
        assert!(message.contains("/problems/not-found"), "{message}");
        assert!(message.contains("no post with that id"), "{message}");
        assert!(message.contains("404"), "{message}");
    }

    /// Yang menjawab bukan API kita — Cloudflare, nginx, atau halaman blokir.
    /// Pesannya harus mengatakan itu, bukan mengarang detail dari badan HTML.
    #[tokio::test]
    async fn a_non_problem_body_still_reports_its_status() {
        let transport = Recorder::answering(502, "<html>Bad gateway</html>");

        let error = client(transport)
            .resolve("https://medium.com/@x/y")
            .await
            .expect_err("502 bukan 200");

        assert!(
            matches!(
                error,
                ApiError::Status {
                    status: 502,
                    problem: None
                }
            ),
            "{error:?}"
        );
        assert!(error.to_string().contains("bukan /api/v1"));
    }

    /// `200` dengan badan yang tidak bisa dibaca adalah kegagalan yang berbeda
    /// dari `200` berisi artikel: yang pertama bug, yang kedua bukan.
    #[tokio::test]
    async fn a_two_hundred_that_is_not_json_is_malformed() {
        let transport = Recorder::answering(200, "{}");

        let error = client(transport)
            .post("e6047997b667")
            .await
            .expect_err("`{}` bukan PostDto");

        assert!(matches!(error, ApiError::Malformed(_)), "{error:?}");
    }

    /// Transport yang gagal tidak berubah jadi status: bot perlu tahu bedanya
    /// antara "server bilang tidak ada" dan "tidak bisa mencapai server".
    #[tokio::test]
    async fn a_transport_failure_stays_a_transport_failure() {
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

        let error = Client::new(Broken, "https://contoh.test", Duration::from_secs(5))
            .post("e6047997b667")
            .await
            .expect_err("transportnya memang rusak");

        assert!(matches!(error, ApiError::Transport(_)), "{error:?}");
    }

    // ---------------------------------------------------------------------
    // Tier tepercaya
    // ---------------------------------------------------------------------

    /// Tanpa token, header itu tidak ada — **bukan** ada tapi kosong.
    ///
    /// Bedanya bukan gaya: server memperlakukan header kosong sebagai klaim
    /// token yang salah dan menjawabnya `401` (`token_tier` di
    /// `freedium-web/src/api/limit.rs`), jadi klien yang mengirim
    /// `X-API-TOKEN:` yang kosong akan berhenti bekerja sama sekali, bukan
    /// mundur ke tier anonim.
    #[tokio::test]
    async fn without_a_token_the_header_is_absent_rather_than_empty() {
        let transport = Recorder::answering(200, "{}");
        let _ = client(transport.clone()).post("e6047997b667").await;

        let sent = transport.sent();
        let names: Vec<&str> = sent[0]
            .headers
            .iter()
            .map(|(name, _)| name.as_str())
            .collect();

        assert_eq!(names, vec!["User-Agent"], "hanya User-Agent yang dikirim");
    }

    #[tokio::test]
    async fn a_configured_token_is_sent_on_every_request() {
        let transport = Recorder::answering(
            200,
            r#"{"schema_version":1,"post_id":"e6047997b667","resolved_url":"https://medium.com/p/e6047997b667"}"#,
        );

        let client = client(transport.clone()).with_api_token(Some("rahasia".to_string()));

        let _ = client.resolve("https://medium.com/@x/y").await;
        let _ = client.post("e6047997b667").await;

        let sent = transport.sent();
        assert_eq!(sent.len(), 2, "satu resolve, satu post");
        for request in &sent {
            let headers = headers(request);
            assert!(
                headers.contains(&("X-API-TOKEN".to_string(), "rahasia".to_string())),
                "{:?}",
                headers
            );
        }
    }

    #[test]
    fn the_page_url_is_ours_not_the_articles_canonical_one() {
        let transport = Recorder::answering(200, "");
        let client = client(transport);

        assert_eq!(
            client.page_url("e6047997b667"),
            "https://contoh.test/p/e6047997b667",
            "trailing slash di base tidak boleh jadi `//p/`"
        );
    }
}
