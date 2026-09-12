//! Konfigurasi runtime, dibaca dari lingkungan saat boot.
//!
//! # `std::env` saja, tanpa pembaca `.env`
//!
//! Sama seperti `freedium-web/src/config.rs`, dan dengan alasan yang sama:
//! `.dockerignore` membuang `.env`, jadi di dalam kontainer file itu memang tidak
//! ada dan lingkungan adalah satu-satunya sumber. Membaca file juga berarti
//! perilakunya bergantung pada direktori kerja proses dijalankan — paritas yang
//! salah.
//!
//! # Satu variabel untuk host, bukan dua
//!
//! [`Config::base_url`] dipakai untuk dua hal: `{base}/api/v1` yang dipanggil
//! bot, dan `{base}/p/{id}` yang ditautkan di akhir artikel yang dipotong.
//! Memisahkannya jadi `FREEDIUM_API_BASE` dan `FREEDIUM_HOST_ADDRESS` akan
//! menghasilkan dua knop yang di setiap deployment nyata diisi nilai yang sama —
//! dan satu kesempatan untuk membuat tautan "baca selengkapnya" menunjuk host
//! yang bukan host yang membacanya.
//!
//! # Token tidak wajib di sini
//!
//! [`Config::telegram_token`] `None` bukan kesalahan boot. `render` dan
//! `healthcheck` tidak butuh token, dan `render` justru alat verifikasi utama —
//! menolak jalan tanpa token berarti alat itu tidak bisa dipakai sebelum ada
//! token. Yang menolak adalah `run`, di [`crate::bot`], dan di sana token memang
//! satu-satunya hal yang membuat loop-nya berarti.
//!
//! # Dua token, dua kegunaan, dan satu nama yang dipakai bersama
//!
//! [`Config::api_token`] adalah `API_TOKEN` — **nama yang sama** dengan yang
//! dibaca `freedium-web/src/config.rs`, dan itu disengaja: satu rahasia, satu
//! baris di `.env`, dibaca kedua sisi. Memberinya nama sendiri di sini
//! (`FREEDIUM_API_TOKEN`) berarti dua nilai yang harus diisi sama, dan satu
//! kesempatan untuk membuat keduanya berbeda tanpa ada yang memberi tahu.
//!
//! `None` berarti header tidak dikirim sama sekali, bukan header kosong. Server
//! memperlakukan token kosong sebagai "tier tidak ada", dan mengirim
//! `X-API-TOKEN: ` akan ditolak `401` oleh server yang mengaktifkannya.

use std::time::Duration;

use thiserror::Error;

/// Alamat produksi. Bukan `freedium.cfd`: bot ini dikembangkan bersama
/// `medium.piyann.my.id`, dan default yang menunjuk instance orang lain akan
/// membuat uji coba lokal diam-diam memanggil server yang tidak kita kontrol.
pub const DEFAULT_BASE_URL: &str = "https://medium.piyann.my.id";

/// Lama `getUpdates` menahan permintaan. Telegram menerima sampai 50 detik;
/// 30 detik adalah yang disarankan dokumennya sendiri, dan lebih pendek berarti
/// lebih banyak permintaan kosong per menit.
pub const DEFAULT_POLL_TIMEOUT: Duration = Duration::from_secs(30);

/// Batas satu panggilan HTTP ke API sendiri.
///
/// Lebih pendek dari timeout server (38 detik di `freedium-web`), dan itu
/// disengaja: kalau server masih sibuk mengambil artikel dari Medium, bot
/// menyerah lebih dulu dan memberi tahu pengguna, alih-alih menahan pesan yang
/// sama di antrean.
pub const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(20);

/// Berapa pesan yang diproses bersamaan.
///
/// Kecil, dan bukan karena kehati-hatian: satu artikel berarti satu permintaan
/// ke API sendiri, yang kalau cache-nya kosong berarti satu pengambilan ke
/// Medium lewat satu exit WARP. Empat sudah lebih dari cukup untuk trafik satu
/// digit pesan per menit, dan cukup kecil untuk tidak pernah jadi penyebab
/// budget pengambilan habis.
pub const DEFAULT_CONCURRENCY: usize = 4;

/// Kenapa bot tidak bisa dikonfigurasi.
///
/// Ini kegagalan boot, bukan kegagalan permintaan: prosesnya berhenti daripada
/// berjalan dengan nilai yang ditebak.
#[derive(Debug, Error)]
pub enum ConfigError {
    /// Sebuah variabel diisi tapi bukan tipe yang diklaimnya.
    #[error("{name} berisi {value:?}, yang bukan {expected} yang sah")]
    Invalid {
        name: &'static str,
        value: String,
        expected: &'static str,
    },
}

/// Semua yang dibaca bot dari lingkungan, diselesaikan sekali saat boot.
#[derive(Debug, Clone)]
pub struct Config {
    /// `FREEDIUM_BASE_URL` — lihat dokumen modul soal kenapa cuma satu.
    pub base_url: String,
    /// `TELEGRAM_ARTICLE_BOT_TOKEN`.
    ///
    /// **Bukan `TELEGRAM_BOT_TOKEN`.** Variabel itu milik notifier alert di
    /// `freedium-web/src/notify.rs`, dan di `.env` produksi sengaja dikosongkan
    /// karena alert-nya membanjir dari pemindai internet. Memakai nama yang sama
    /// berarti menyalakan bot ini sekaligus menyalakan kembali banjir itu — dan
    /// dua pemanggil `getUpdates` dengan token yang sama akan saling menendang
    /// dengan `409 Conflict`.
    pub telegram_token: Option<String>,
    /// `API_TOKEN` — identitas klien tepercaya di `/api/v1`. Lihat catatan modul
    /// soal kenapa namanya sama dengan milik server.
    ///
    /// Tanpa ini bot memakai bucket anonim: 3 permintaan cache-miss per menit
    /// dengan **burst 1**, yang berarti artikel kedua yang dikirim berurutan
    /// dalam dua puluh detik ditolak `429`. Itu bukan pembatasan yang masuk akal
    /// untuk satu orang yang mengirim tautan ke dirinya sendiri.
    pub api_token: Option<String>,
    pub poll_timeout: Duration,
    pub request_timeout: Duration,
}

impl Config {
    /// Membaca lingkungan. Satu-satunya konstruktor.
    pub fn from_env() -> Result<Self, ConfigError> {
        Self::from_lookup(|name| std::env::var(name).ok())
    }

    /// Isi [`Self::from_env`], tapi sumbernya disuntikkan.
    ///
    /// Terpisah karena `unsafe_code = "forbid"` membuat `env::set_var` tidak
    /// bisa dipakai di test, jadi satu-satunya cara menguji pembacaan lingkungan
    /// adalah tidak membacanya.
    pub fn from_lookup(lookup: impl Fn(&str) -> Option<String>) -> Result<Self, ConfigError> {
        let base_url =
            non_empty(lookup("FREEDIUM_BASE_URL")).unwrap_or(DEFAULT_BASE_URL.to_string());

        Ok(Self {
            // `trim_end_matches('/')` supaya `{base}/api/v1` tidak jadi
            // `//api/v1` ketika seseorang menulis trailing slash di `.env` —
            // dan Cloudflare meneruskan `//` apa adanya, jadi itu 404.
            base_url: base_url.trim_end_matches('/').to_string(),
            telegram_token: non_empty(lookup("TELEGRAM_ARTICLE_BOT_TOKEN")),
            api_token: non_empty(lookup("API_TOKEN")),
            poll_timeout: seconds(
                lookup("BOT_POLL_TIMEOUT_SECONDS"),
                DEFAULT_POLL_TIMEOUT,
                "BOT_POLL_TIMEOUT_SECONDS",
            )?,
            request_timeout: seconds(
                lookup("BOT_REQUEST_TIMEOUT_SECONDS"),
                DEFAULT_REQUEST_TIMEOUT,
                "BOT_REQUEST_TIMEOUT_SECONDS",
            )?,
        })
    }
}

/// Sebuah variabel yang ada tapi kosong sama dengan tidak ada — `.env` yang
/// memuat `TELEGRAM_ARTICLE_BOT_TOKEN=` adalah hal yang wajar ditulis, dan
/// memperlakukannya sebagai token `""` akan menghasilkan `getMe` yang gagal
/// dengan pesan Telegram, bukan dengan pesan kita.
fn non_empty(value: Option<String>) -> Option<String> {
    value.filter(|text| !text.trim().is_empty())
}

fn seconds(
    value: Option<String>,
    fallback: Duration,
    name: &'static str,
) -> Result<Duration, ConfigError> {
    let Some(text) = non_empty(value) else {
        return Ok(fallback);
    };

    text.trim()
        .parse::<u64>()
        .map(Duration::from_secs)
        .map_err(|_| ConfigError::Invalid {
            name,
            value: text,
            expected: "jumlah detik",
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config_from(pairs: &[(&str, &str)]) -> Result<Config, ConfigError> {
        let pairs: Vec<(String, String)> = pairs
            .iter()
            .map(|(name, value)| (name.to_string(), value.to_string()))
            .collect();
        Config::from_lookup(|name| {
            pairs
                .iter()
                .find(|(key, _)| key == name)
                .map(|(_, value)| value.clone())
        })
    }

    #[test]
    fn an_empty_environment_gives_the_defaults() {
        let config = config_from(&[]).expect("lingkungan kosong tetap sah");

        assert_eq!(config.base_url, DEFAULT_BASE_URL);
        assert_eq!(config.telegram_token, None);
        assert_eq!(config.api_token, None);
        assert_eq!(config.poll_timeout, DEFAULT_POLL_TIMEOUT);
        assert_eq!(config.request_timeout, DEFAULT_REQUEST_TIMEOUT);
    }

    /// Nama variabelnya harus sama dengan yang dibaca `freedium-web`, karena
    /// keduanya adalah satu rahasia yang sama. Test ini gagal kalau salah satu
    /// sisi mengganti namanya sendiri.
    #[test]
    fn the_api_token_is_the_same_variable_the_server_reads() {
        let config = config_from(&[("API_TOKEN", "rahasia")]).expect("sah");
        assert_eq!(config.api_token.as_deref(), Some("rahasia"));

        let kosong = config_from(&[("API_TOKEN", "  ")]).expect("sah");
        assert_eq!(
            kosong.api_token, None,
            "token kosong berarti header tidak dikirim, bukan header kosong"
        );
    }

    /// **Token bot artikel tidak boleh bisa tertukar dengan token notifier.**
    ///
    /// `TELEGRAM_BOT_TOKEN` ada di `.env` produksi dan sengaja dikosongkan;
    /// kalau bot ini membacanya, alert admin menyala kembali dan dua pemanggil
    /// `getUpdates` bertabrakan. Test ini gagal kalau nama variabelnya berubah.
    #[test]
    fn the_article_token_is_not_the_notifiers_token() {
        let config = config_from(&[
            ("TELEGRAM_BOT_TOKEN", "token-notifier"),
            ("TELEGRAM_ARTICLE_BOT_TOKEN", "token-artikel"),
        ])
        .expect("sah");

        assert_eq!(config.telegram_token.as_deref(), Some("token-artikel"));

        let only_notifier = config_from(&[("TELEGRAM_BOT_TOKEN", "token-notifier")]).expect("sah");
        assert_eq!(
            only_notifier.telegram_token, None,
            "token notifier tidak boleh menyalakan bot ini"
        );
    }

    #[test]
    fn an_empty_value_reads_as_unset() {
        let config = config_from(&[
            ("TELEGRAM_ARTICLE_BOT_TOKEN", "   "),
            ("FREEDIUM_BASE_URL", ""),
        ])
        .expect("sah");

        assert_eq!(config.telegram_token, None);
        assert_eq!(config.base_url, DEFAULT_BASE_URL);
    }

    /// Cloudflare meneruskan `//api/v1` apa adanya, jadi trailing slash bukan
    /// kerapian melainkan 404.
    #[test]
    fn a_trailing_slash_is_stripped_from_the_base_url() {
        let config = config_from(&[("FREEDIUM_BASE_URL", "https://contoh.test///")]).expect("sah");
        assert_eq!(config.base_url, "https://contoh.test");
    }

    #[test]
    fn a_duration_that_is_not_a_number_is_a_boot_failure() {
        let error =
            config_from(&[("BOT_POLL_TIMEOUT_SECONDS", "tiga puluh")]).expect_err("bukan angka");
        assert!(
            matches!(error, ConfigError::Invalid { name, .. } if name == "BOT_POLL_TIMEOUT_SECONDS")
        );
        assert!(error.to_string().contains("tiga puluh"));
    }
}
