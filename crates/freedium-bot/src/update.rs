//! Sepotong `Update` Telegram, dan pencarian URL di dalamnya.
//!
//! # Kenapa tipenya ditulis tangan dan bukan diambil dari crate
//!
//! Sama seperti [`crate::rich::model`]: yang dibutuhkan dari `Update` cuma
//! empat field, dan satu-satunya cara mendapatkannya dari sebuah crate Bot API
//! adalah membawa seluruh crate itu — yang tetap tidak punya `sendRichMessage`.
//!
//! # `offset` dan `length` sebuah entity bukan indeks byte
//!
//! Telegram menghitungnya dalam **satuan kode UTF-16**, sama seperti ia
//! menghitung panjang pesan. Memotong `&text[offset..offset + length]` karena itu
//! salah begitu ada emoji: satu emoji adalah satu karakter Rust, dua satuan
//! UTF-16, dan empat byte. [`MessageEntity::slice`] mengurusnya.
//!
//! Satuan yang sama muncul di [`crate::rich::budget`], dan itu bukan kebetulan:
//! Telegram menghitung teksnya dengan satu cara di seluruh API-nya.
//!
//! # Dua jalur masuk, dan kenapa yang kedua tidak bisa ditambahkan di Telegram
//!
//! Sebuah pesan bisa datang dari obrolan dengan bot, atau dari **panggilan
//! tamu** — seseorang menulis `@bot` di obrolan yang bukan milik bot, dan bot
//! menjawab sekali di sana. Yang kedua adalah fitur Telegram (Bot API 10.0),
//! bukan sesuatu yang bisa dinyalakan dari sisi klien: benderanya ada di
//! BotFather, dan `getMe` melaporkannya sebagai `supports_guest_queries`.
//!
//! Yang **bisa** salah dari sisi kita cuma satu: `allowed_updates`. Tanpa
//! `guest_message` di daftar itu, Telegram berhenti mengirimkannya sama sekali —
//! tidak ada galat, tidak ada log, hanya bot yang diam ketika dipanggil. Itu
//! kegagalan yang terlihat persis seperti "fitur ini belum ada di server", dan
//! karena itu [`crate::telegram::Client::get_updates`] menyebut keduanya.

use serde::Deserialize;

/// Dari mana sebuah pesan datang, dan lewat apa balasannya.
///
/// Perbedaan ini bukan detail: pesan langsung dibalas `sendRichMessage` ke
/// sebuah `chat_id`, sedangkan panggilan tamu dibalas `answerGuestQuery` dengan
/// sebuah `guest_query_id` dan **tidak punya `chat_id` sama sekali**. Menjawab
/// yang kedua dengan cara pertama berarti mengirim pesan kedua ke obrolan orang
/// — persis yang tidak diminta.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source<'a> {
    /// Obrolan dengan bot, atau grup yang menyebut botnya.
    Direct,
    /// Dipanggil di obrolan yang bukan milik bot. Isinya `guest_query_id`.
    Guest(&'a str),
}

/// Satu pembaruan dari `getUpdates`.
///
/// `message` dan `guest_message` dideklarasikan; sisanya tidak. Telegram mengirim
/// jenis pembaruan lain (edit, callback, poll), dan `deny_unknown_fields` tidak
/// dipakai justru supaya semuanya bisa diabaikan dengan tenang — bot ini tidak
/// punya urusan dengan pesan yang sudah diedit.
///
/// `guest_message` isinya bentuk [`Message`] yang sama, dengan satu field
/// tambahan. Itu memang begitu di servernya: `Client.cpp:19503` menyalurkannya
/// lewat `add_message_update` yang sama dengan pesan biasa.
#[derive(Debug, Clone, Deserialize)]
pub struct Update {
    pub update_id: i64,
    #[serde(default)]
    pub message: Option<Message>,
    #[serde(default)]
    pub guest_message: Option<Message>,
}

impl Update {
    /// Pesan yang perlu ditangani, dari jalur mana pun.
    ///
    /// Mengambil alih alih meminjam: pemanggilnya memang tidak butuh
    /// pembaruan ini lagi setelah pesannya diambil, dan meminjam berarti satu
    /// salinan [`Message`] per pesan hanya supaya bisa masuk `tokio::spawn`.
    ///
    /// `message` lebih dulu kalau keduanya ada. Keduanya sekaligus tidak
    /// seharusnya terjadi, dan memilih satu dengan urutan yang tetap lebih baik
    /// daripada memproses keduanya dan mengirim artikel dua kali.
    #[must_use]
    pub fn into_incoming(self) -> Option<Message> {
        self.message.or(self.guest_message)
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct Message {
    pub message_id: i64,
    pub chat: Chat,
    /// Tidak ada untuk pesan yang bukan teks — foto, stiker, suara.
    #[serde(default)]
    pub text: Option<String>,
    /// Terurut menaik menurut `offset`, dan itu yang membuat
    /// [`url_candidates`] bisa mengandalkan urutannya.
    #[serde(default)]
    pub entities: Vec<MessageEntity>,
    /// Hadir **hanya** pada panggilan tamu, dan itulah satu-satunya penanda
    /// bahwa pesan ini bukan pesan biasa.
    ///
    /// Sebuah string, bukan angka, meskipun isinya angka: server menulisnya
    /// dengan `td::to_string` (`Client.cpp:4847`) dan membacanya kembali dengan
    /// `td::to_integer` (`Client.cpp:15434`). Menyimpannya apa adanya berarti
    /// bot ini tidak perlu tahu bahwa ia sebenarnya sebuah bilangan.
    #[serde(default)]
    pub guest_query_id: Option<String>,
}

impl Message {
    /// Lewat apa pesan ini harus dibalas.
    #[must_use]
    pub fn source(&self) -> Source<'_> {
        match self.guest_query_id.as_deref() {
            Some(query_id) => Source::Guest(query_id),
            None => Source::Direct,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct Chat {
    pub id: i64,
}

/// Satu rentang beranotasi di dalam [`Message::text`].
#[derive(Debug, Clone, Deserialize)]
pub struct MessageEntity {
    #[serde(rename = "type")]
    pub kind: String,
    pub offset: usize,
    pub length: usize,
    /// Tujuan sebuah `text_link` — teks yang tampil berbeda dari alamat yang
    /// dituju. `None` untuk tipe lain.
    #[serde(default)]
    pub url: Option<String>,
}

impl MessageEntity {
    /// Teks yang ditunjuk entity ini, dipotong menurut satuan UTF-16.
    ///
    /// `None` kalau rentangnya tidak masuk akal — di luar teks, atau memotong
    /// di tengah pasangan surrogate. Keduanya berarti Telegram mengirim sesuatu
    /// yang tidak kita pahami, dan menebak lebih buruk daripada mengabaikannya.
    #[must_use]
    fn slice<'a>(&self, text: &'a str) -> Option<&'a str> {
        let end = self.offset.checked_add(self.length)?;

        // Batasnya dicari di dalam teks aslinya, bukan dengan membangun `String`
        // baru dari potongan `u16`-nya: yang diinginkan adalah potongan teks
        // asli, dan `from_utf16` akan menolak rentang yang memotong pasangan
        // surrogate — persis kasus yang harus ditolak.
        let start = byte_offset(text, self.offset)?;
        let end = byte_offset(text, end)?;

        text.get(start..end)
    }
}

/// Indeks byte tempat satuan UTF-16 ke-`units` dimulai.
///
/// `None` kalau `units` jatuh di tengah karakter (tengah pasangan surrogate),
/// atau melewati akhir teks.
fn byte_offset(text: &str, units: usize) -> Option<usize> {
    let mut seen = 0;

    for (byte_index, character) in text.char_indices() {
        if seen == units {
            return Some(byte_index);
        }
        seen += character.len_utf16();
        if seen > units {
            // Melewatinya berarti `units` ada di tengah karakter ini.
            return None;
        }
    }

    (seen == units).then_some(text.len())
}

/// Apakah entitas ini sebuah tautan, dan apa tujuannya.
///
/// `text_link` membawa tujuannya di field `url` dan teksnya di badan pesan;
/// `url` adalah kebalikannya — teksnya **adalah** tujuannya.
fn entity_url<'a>(entity: &'a MessageEntity, text: &'a str) -> Option<&'a str> {
    match entity.kind.as_str() {
        // Teksnya berbeda dari tujuannya, jadi tujuannya harus dibaca dari
        // field. Ini bentuk yang paling sering dipakai klien untuk membagikan
        // tautan, dan mengabaikannya berarti bot melewatkan tautan yang jelas.
        "text_link" => entity.url.as_deref().filter(|url| !url.is_empty()),
        "url" => entity.slice(text),
        _ => None,
    }
}

/// Kandidat URL di dalam sebuah pesan, terurut sebagaimana penulisnya menulis.
///
/// # Kenapa entity lebih dulu, dan teks polos cuma cadangan
///
/// Entity adalah apa yang Telegram **tahu** itu tautan. Membaca teks polos
/// sebagai gantinya berarti bot akan mengambil hal yang kebetulan berbentuk URL
/// di mata sebuah pemisah spasi. Jadi entity dipakai lebih dulu; kalau pesannya
/// tidak punya satu pun, teksnya dipecah sebagai usaha terakhir — dan itu benar-
/// benar usaha terakhir, karena klien yang mengirim `entities` kosong untuk
/// pesan bertaut tetap ada.
///
/// # Yang tidak disaring di sini
///
/// Apakah ini URL Medium yang sah. Itu pertanyaan `/api/v1/resolve`, dan
/// menjawabnya di sini berarti implementasi kedua dari aturan yang sama. Yang
/// dikembalikan fungsi ini adalah **kandidat**; pemanggil mencobanya satu per
/// satu dan berhenti di yang pertama berhasil.
#[must_use]
pub fn url_candidates(message: &Message) -> Vec<String> {
    let Some(text) = message.text.as_deref() else {
        return Vec::new();
    };

    let from_entities: Vec<String> = message
        .entities
        .iter()
        .filter_map(|entity| entity_url(entity, text))
        .map(str::to_string)
        .collect();

    if !from_entities.is_empty() {
        return from_entities;
    }

    text.split_whitespace()
        .filter_map(|token| bare_candidate(token).map(str::to_string))
        .collect()
}

/// Satu kata dari teks polos, kalau ia memang bisa jadi URL atau id post.
///
/// Tanda baca di **kedua** ujung dibuang: `lihat https://x.test/a, bagus` dan
/// `(https://x.test/a)` adalah dua hal yang wajar ditulis, dan koma atau kurung
/// yang ikut terbawa membuat URL-nya tidak ditemukan sama sekali — yang terlihat
/// seperti bot yang tidak mengenali tautannya, bukan seperti tanda baca.
fn bare_candidate(token: &str) -> Option<&str> {
    let token = token.trim_matches([
        '.', ',', ';', ':', '!', '?', '(', ')', '[', ']', '<', '>', '"', '\'',
    ]);

    if token.starts_with("https://") || token.starts_with("http://") {
        return Some(token);
    }

    // Id telanjang juga diterima, karena `/api/v1/resolve` menerimanya dan
    // orang memang menempelkan id begitu saja. [`crate::api::bare_post_id`]
    // yang memutuskan bentuknya, bukan salinan aturan di sini.
    crate::api::bare_post_id(token)
}

/// Nama perintah di awal pesan, tanpa garis miring dan tanpa `@nama_bot`.
///
/// `None` kalau pesannya bukan perintah. `@nama_bot` dibuang karena di grup
/// klien menulis `/artikel@freedium_bot`, dan membandingkan `artikel@freedium_bot`
/// dengan `artikel` akan selalu meleset.
#[must_use]
pub fn command(text: &str) -> Option<&str> {
    let rest = text.trim_start().strip_prefix('/')?;
    // Perintah berhenti di spasi pertama: `/artikel https://…`.
    let name = rest.split_whitespace().next()?;
    let name = name.split('@').next()?;

    (!name.is_empty()).then_some(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn message(text: &str, entities: Vec<MessageEntity>) -> Message {
        Message {
            message_id: 1,
            chat: Chat { id: 42 },
            text: Some(text.to_string()),
            entities,
            guest_query_id: None,
        }
    }

    fn entity(kind: &str, offset: usize, length: usize, url: Option<&str>) -> MessageEntity {
        MessageEntity {
            kind: kind.to_string(),
            offset,
            length,
            url: url.map(str::to_string),
        }
    }

    // ---------------------------------------------------------------------
    // Pemotongan entity
    // ---------------------------------------------------------------------

    #[test]
    fn an_entity_slices_the_text_it_points_at() {
        let text = "lihat https://medium.com/@x/y ya";
        let url = entity("url", 6, 23, None);

        assert_eq!(url.slice(text), Some("https://medium.com/@x/y"));
    }

    /// **Ini kenapa pemotongan byte tidak dipakai.** Satu emoji adalah satu
    /// karakter Rust tapi dua satuan UTF-16; indeks Telegram menghitung yang
    /// kedua. Memotong `&text[6..24]` di sini akan mengambil teks yang salah —
    /// dan pada teks yang lebih panjang, akan panik di tengah karakter.
    #[test]
    fn an_entity_offset_counts_utf16_units_not_bytes_or_chars() {
        // Dua emoji = 4 satuan UTF-16, lalu satu spasi di satuan 4.
        let text = "🎉🎉 https://x.test/a";
        let url = entity("url", 5, 16, None);

        assert_eq!(url.slice(text), Some("https://x.test/a"));
        // Yang salah, ditunjukkan supaya jelas apa yang dihindari: dipakai
        // sebagai indeks byte, offset 5 jatuh di tengah emoji kedua, dan
        // potongan seperti itu tidak ada.
        assert_eq!(text.get(5..21), None, "offset UTF-16 bukan offset byte");
    }

    #[test]
    fn a_slice_that_cuts_a_character_in_half_is_refused() {
        // Satu emoji menempati satuan 0 dan 1; mulai dari 1 berarti di tengahnya.
        let text = "🎉x";
        assert_eq!(entity("url", 1, 1, None).slice(text), None);
    }

    #[test]
    fn a_slice_past_the_end_of_the_text_is_refused() {
        let text = "pendek";
        assert_eq!(entity("url", 0, 99, None).slice(text), None);
        assert_eq!(entity("url", 99, 1, None).slice(text), None);
    }

    #[test]
    fn the_empty_slice_at_the_very_end_is_allowed() {
        let text = "abc";
        assert_eq!(entity("url", 3, 0, None).slice(text), Some(""));
    }

    // ---------------------------------------------------------------------
    // Kandidat
    // ---------------------------------------------------------------------

    #[test]
    fn a_url_entity_is_taken_from_the_text() {
        let text = "baca https://medium.com/@x/y";
        let found = url_candidates(&message(text, vec![entity("url", 5, 23, None)]));

        assert_eq!(found, ["https://medium.com/@x/y"]);
    }

    /// Teks yang tampil dan tujuan sebuah `text_link` berbeda, dan yang
    /// dibutuhkan bot adalah tujuannya.
    #[test]
    fn a_text_link_entity_is_taken_from_its_destination_not_its_label() {
        let text = "artikel bagus";
        let found = url_candidates(&message(
            text,
            vec![entity("text_link", 0, 13, Some("https://medium.com/@x/y"))],
        ));

        assert_eq!(found, ["https://medium.com/@x/y"]);
    }

    #[test]
    fn entities_of_other_kinds_are_ignored() {
        let text = "/artikel @seseorang";
        let found = url_candidates(&message(
            text,
            vec![
                entity("bot_command", 0, 8, None),
                entity("mention", 9, 10, None),
            ],
        ));

        // Tidak ada entity tautan, jadi teksnya jatuh ke pemecahan kata — dan
        // tidak ada kata yang berbentuk URL atau id.
        assert!(found.is_empty(), "{found:?}");
    }

    /// Klien yang tidak mengirim `entities` tetap ada, dan itu satu-satunya
    /// alasan pemecahan teks polos ada.
    #[test]
    fn without_entities_the_text_is_split_on_whitespace() {
        let found = url_candidates(&message(
            "lihat https://medium.com/@x/y dan https://medium.com/@x/z",
            Vec::new(),
        ));

        assert_eq!(
            found,
            ["https://medium.com/@x/y", "https://medium.com/@x/z"]
        );
    }

    #[test]
    fn trailing_punctuation_is_not_part_of_the_url() {
        let found = url_candidates(&message(
            "baca (https://medium.com/@x/y), lalu ini https://medium.com/@x/z.",
            Vec::new(),
        ));

        assert_eq!(
            found,
            ["https://medium.com/@x/y", "https://medium.com/@x/z"],
            "kurung dan titik tidak boleh ikut terbawa"
        );
    }

    /// Id telanjang diterima, karena itu bentuk yang `resolve` sendiri terima.
    #[test]
    fn a_bare_post_id_in_the_text_is_a_candidate() {
        let found = url_candidates(&message("e6047997b667", Vec::new()));
        assert_eq!(found, ["e6047997b667"]);
    }

    #[test]
    fn a_message_with_no_text_has_no_candidates() {
        let mut message = message("", Vec::new());
        message.text = None;

        assert!(url_candidates(&message).is_empty());
    }

    /// Entity yang menang: teks polos tidak ikut diperiksa ketika sudah ada
    /// entity, supaya satu tautan tidak muncul dua kali.
    #[test]
    fn entities_suppress_the_plain_text_fallback_entirely() {
        let text = "https://medium.com/@x/y";
        let found = url_candidates(&message(text, vec![entity("url", 0, 25, None)]));

        assert_eq!(found, ["https://medium.com/@x/y"], "sekali, bukan dua kali");
    }

    // ---------------------------------------------------------------------
    // Perintah
    // ---------------------------------------------------------------------

    #[test]
    fn a_command_is_recognised_with_or_without_a_bot_name() {
        assert_eq!(command("/start"), Some("start"));
        assert_eq!(command("/artikel https://x.test"), Some("artikel"));
        assert_eq!(command("/artikel@freedium_bot"), Some("artikel"));
        assert_eq!(command("  /help  "), Some("help"));
    }

    #[test]
    fn text_that_is_not_a_command_has_none() {
        for not_a_command in ["", "halo", "https://medium.com/@x/y", "/", "@/x"] {
            assert_eq!(command(not_a_command), None, "{not_a_command:?}");
        }
    }

    /// Garis miring di tengah kalimat bukan perintah: `/` di jalur sebuah URL
    /// adalah hal yang paling sering muncul, dan pesan yang berisi tautan tidak
    /// boleh diperlakukan sebagai perintah.
    #[test]
    fn a_slash_inside_a_message_is_not_a_command() {
        assert_eq!(command("lihat https://medium.com/@x/y"), None);
    }

    // ---------------------------------------------------------------------
    // Deserialisasi
    // ---------------------------------------------------------------------

    /// Bentuk `Update` yang benar-benar dikirim Telegram, dipersempit ke field
    /// yang dipakai. `update_id` dan `message.chat.id` selalu ada; sisanya
    /// opsional.
    #[test]
    fn an_update_deserialises_from_the_wire_shape() {
        let raw = r#"{
            "update_id": 81234567,
            "message": {
                "message_id": 5,
                "date": 1789000000,
                "chat": { "id": -1001234567890, "type": "group" },
                "from": { "id": 7, "is_bot": false, "first_name": "A" },
                "text": "https://medium.com/@x/y",
                "entities": [
                    { "type": "url", "offset": 0, "length": 23 }
                ]
            }
        }"#;

        let update: Update = serde_json::from_str(raw).expect("bentuk sungguhan bisa dibaca");
        let message = update.message.expect("ada pesannya");

        assert_eq!(update.update_id, 81_234_567);
        assert_eq!(message.chat.id, -1_001_234_567_890);
        assert_eq!(url_candidates(&message), ["https://medium.com/@x/y"]);
    }

    /// Pembaruan yang bukan pesan — edit, callback, poll — harus terlewat
    /// dengan tenang, bukan menggagalkan pembacaan seluruh batch.
    #[test]
    fn an_update_without_a_message_deserialises() {
        let update: Update =
            serde_json::from_str(r#"{"update_id":1,"edited_message":{"message_id":1}}"#)
                .expect("field yang tidak dikenal diabaikan");

        assert!(update.message.is_none());
    }

    // ---------------------------------------------------------------------
    // Panggilan tamu
    // ---------------------------------------------------------------------

    /// Bentuk `guest_message`, dipersempit ke field yang dipakai.
    ///
    /// `guest_query_id` ditulis sebagai **string** meskipun isinya angka, dan
    /// itu bukan pilihan kita: server menuliskannya dengan `td::to_string`
    /// (`Client.cpp:4847`). Membacanya sebagai `i64` akan menggagalkan
    /// pembacaan seluruh batch, bukan cuma satu pembaruan.
    #[test]
    fn a_guest_message_deserialises_with_its_query_id() {
        let raw = r#"{
            "update_id": 900,
            "guest_message": {
                "message_id": 12,
                "date": 1789000000,
                "chat": { "id": -1009876543210, "type": "supergroup" },
                "from": { "id": 7, "is_bot": false, "first_name": "A" },
                "guest_query_id": "8239871239",
                "text": "https://medium.com/@x/y",
                "entities": [ { "type": "url", "offset": 0, "length": 23 } ]
            }
        }"#;

        let update: Update = serde_json::from_str(raw).expect("bentuk sungguhan bisa dibaca");
        assert!(update.message.is_none(), "bukan pesan biasa");

        let message = update.into_incoming().expect("ada pesannya");
        assert_eq!(message.source(), Source::Guest("8239871239"));
        assert_eq!(url_candidates(&message), ["https://medium.com/@x/y"]);
    }

    /// Tanpa `guest_query_id`, sebuah pesan adalah pesan biasa — dan jalur
    /// itulah yang menentukan [`Source::Direct`], bukan ada tidaknya
    /// `guest_message` di amplopnya.
    #[test]
    fn a_message_without_a_query_id_is_direct() {
        let ordinary = message("halo", Vec::new());
        assert_eq!(ordinary.source(), Source::Direct);

        // Bahkan di dalam `guest_message`: yang menentukan adalah fieldnya,
        // karena itu satu-satunya yang memberi tahu ke mana balasannya pergi.
        let without =
            serde_json::from_str::<Message>(r#"{"message_id":1,"chat":{"id":42}}"#).expect("sah");
        assert_eq!(without.source(), Source::Direct);
    }

    /// Pembaruan tanpa pesan sama sekali tetap harus terlewat dengan tenang —
    /// termasuk bentuk yang cuma punya `guest_message` yang bukan milik kita.
    #[test]
    fn an_update_carrying_neither_kind_of_message_has_nothing_incoming() {
        let update: Update = serde_json::from_str(r#"{"update_id":1,"callback_query":{"id":"x"}}"#)
            .expect("field yang tidak dikenal diabaikan");

        assert!(update.into_incoming().is_none());
    }

    #[test]
    fn a_message_without_text_deserialises() {
        let raw = r#"{"message_id":1,"chat":{"id":42},"sticker":{"file_id":"x"}}"#;
        let message: Message = serde_json::from_str(raw).expect("pesan non-teks tetap sah");

        assert!(message.text.is_none());
        assert!(message.entities.is_empty());
    }
}
