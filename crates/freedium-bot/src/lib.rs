//! Bot Telegram: membaca artikel Medium sebagai *rich message*.
//!
//! # Kenapa ini ada
//!
//! `medium.piyann.my.id` merender artikel dengan baik, tapi membacanya berarti
//! membuka browser. Sejak **Bot API 10.1** (11 Jun 2026) Telegram punya
//! `sendRichMessage`, yang `blocks`-nya adalah keluarga `pageBlock` TDLib —
//! heading, list, `<details>`, math, media. Itu cukup untuk mengirim artikel
//! sebagai dokumen, bukan sebagai tempelan teks yang kehilangan struktur.
//!
//! # Bentuknya: klien tipis, bukan renderer kedua
//!
//! Bot ini **memanggil API publik sendiri** (`/api/v1`) alih-alih menautkan
//! `medium-doc`/`medium-render` dan merender dari IR. Bedanya bukan kecepatan,
//! tapi apa yang harus hidup sebelum bot bisa hidup: `freedium-cache` dan
//! `medium-client` adalah **syarat boot** — `AppState::new` melakukan `init_db`
//! dan handshake Redis di awal. Merender dari IR berarti bot mati kalau Redis
//! mati, padahal tugasnya cuma mengubah URL jadi pesan. Lewat HTTP, satu-satunya
//! ketergantungannya adalah satu endpoint yang sudah dimonitor.
//!
//! Harga yang dibayar adalah satu lompatan HTTP, dan proyeksi DTO yang lossy.
//! Yang hilang (`HeadingSpacing`, `ParagraphMargin`, id gambar mentah, markup di
//! dalam `PRE`) semuanya hal yang rich message Telegram memang tidak punya.
//!
//! # Peta modul
//!
//! | Modul | Isinya |
//! |---|---|
//! | [`config`] | `std::env` saja — tidak ada dotenv, sama seperti `freedium-web` |
//! | [`api`] | Klien `/api/v1`: `resolve`, `post`, dan ekstraksi id lokal |
//! | [`telegram`] | Klien Bot API: `getUpdates`, `getMe`, `sendRichMessage` |
//! | [`update`] | `Update`/`Message` seperlunya, dan pencarian URL Medium |
//! | [`rich`] | `BlockDto` → `InputRichBlock`, anggaran, dan pemotongan |
//! | [`bot`] | Loop long polling, dan apa yang terjadi pada satu pesan |
//!
//! # Yang tidak ada di sini, dan disengaja
//!
//! Tidak ada crate Bot API pihak ketiga. `sendRichMessage` berumur beberapa
//! bulan dan belum ada pustaka yang mengikutinya, jadi tipe yang dibutuhkan
//! tetap harus dideklarasikan sendiri — membayar sebuah framework besar untuk
//! `getUpdates` saja bukan pertukaran yang baik.

pub mod api;
pub mod bot;
pub mod config;
pub mod rich;
pub mod telegram;
pub mod update;
