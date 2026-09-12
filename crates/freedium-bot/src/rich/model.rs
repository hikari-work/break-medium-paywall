//! Bentuk wire `rich_message` — tipe keluaran, bukan masukan.
//!
//! # Kenapa dideklarasikan sendiri
//!
//! `sendRichMessage` baru ada sejak Bot API 10.1 (11 Jun 2026). Belum ada crate
//! Telegram yang mengikutinya — `teloxide` dan kerabatnya masih tertinggal di
//! era `parse_mode` — jadi `InputRichBlock` harus ditulis tangan apa pun yang
//! terjadi. Yang ditolak di sini adalah membayar sebuah framework besar hanya
//! untuk `getUpdates`, lalu tetap menulis tipe ini sendiri.
//!
//! # Kenapa hanya sebagian varian yang ada
//!
//! Bot API mendefinisikan 21 varian `InputRichBlock`; di sini ada sepuluh —
//! yang benar-benar bisa dihasilkan [`crate::rich`], ditambah `divider` yang
//! dipakai pemotongan. Sisanya (`mathematical_expression`, `anchor`, `table`,
//! `details`, `map`, `slideshow`, `thinking`, dan keluarga media kecuali
//! `photo`) sengaja tidak ada: pipeline Medium di repo ini tidak pernah
//! menghasilkan tabel, matematika, atau peta — `medium-doc/src/parse.rs:768`
//! membuang tipe yang tidak dikenal — dan mendeklarasikan varian yang tidak
//! pernah dikirim berarti menebak bentuk field yang tidak ada cara mengujinya.
//!
//! Menambahkannya nanti tidak merusak apa pun: ini tipe keluaran, dan enum yang
//! lebih kecil tidak bisa diam-diam salah.
//!
//! # `skip_entity_detection`
//!
//! [`InputRichMessage::skip_entity_detection`] disetel `true` oleh
//! [`crate::rich::rich_message`], dan itu disengaja: tanpa itu Telegram akan
//! mengubah URL yang muncul di dalam blok `pre` menjadi tautan, dan menemukan
//! `@nama` serta nomor telepon di teks yang tidak pernah memintanya. Artikel
//! yang setia adalah artikel yang isinya persis seperti yang ditulis penulisnya.
//!
//! Harganya: URL yang ditulis penulis sebagai teks polos berhenti bisa
//! disentuh. Itu dapat diterima karena Medium sudah membuat `InlineDto::Link`
//! di mana pun ia sendiri menganggap sesuatu itu tautan.

use serde::Serialize;

/// `type` di dalam [`InputMediaPhoto`].
///
/// Telegram menuntutnya, dan salah tulis di sini adalah penolakan validasi yang
/// pesannya tidak menyebut nama field.
const PHOTO: &str = "photo";

/// Isi `sendRichMessage`'s `rich_message`.
///
/// # Kenapa `html` dan `markdown` tidak ada di sini
///
/// `InputRichMessage` menerima **tepat satu** dari `html`, `markdown`, atau
/// `blocks`. Bot ini selalu mengirim `blocks`, jadi dua field lainnya tidak
/// dideklarasikan sama sekali — bukan di-`None`-kan, tapi tidak ada. Dengan
/// begitu tidak ada cara menuliskan pesan yang menyetel dua mode sekaligus, dan
/// itu satu kelas bug yang hilang sebelum sempat ada.
///
/// `media` juga tidak ada, dan alasannya berbeda: field itu hanya berarti untuk
/// mode `html`/`markdown`, tempat bot harus menyebutkan medianya secara
/// terpisah. Dalam mode `blocks`, media dibawa oleh bloknya sendiri
/// ([`InputRichBlock::Photo`]).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct InputRichMessage {
    pub blocks: Vec<InputRichBlock>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub skip_entity_detection: Option<bool>,
}

impl InputRichMessage {
    /// Pesan dari blok yang sudah jadi.
    ///
    /// `skip_entity_detection` selalu `true` di sini — lihat catatan modul.
    /// Kalau suatu saat Telegram menolak field itu, menjadikannya `None` adalah
    /// satu-satunya perubahan yang diperlukan.
    #[must_use]
    pub fn from_blocks(blocks: Vec<InputRichBlock>) -> Self {
        Self {
            blocks,
            skip_entity_detection: Some(true),
        }
    }
}

/// Satu blok. Diskriminatornya `type`, snake_case.
///
/// Bentuknya sengaja sejajar dengan `pageBlock` TDLib — `paragraph`, `heading`,
/// `pre`, `list`, `blockquote`, `pullquote`, `collage`, `photo` — karena itu
/// memang kosakata yang sama, hanya versi Bot API-nya.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum InputRichBlock {
    /// `<p>`.
    Paragraph { text: RichText },

    /// `<h1>`–`<h6>`. `size` 1 adalah yang terbesar.
    Heading { text: RichText, size: u8 },

    /// `<pre><code>`. `language` yang tidak dikenal Telegram adalah kandidat
    /// pertama penolakan validasi, karena itu ia opsional dan bisa dilepas.
    Pre {
        text: RichText,
        #[serde(skip_serializing_if = "Option::is_none")]
        language: Option<String>,
    },

    /// `<footer>` — baris byline di akhir artikel.
    Footer { text: RichText },

    /// `<hr/>`. Tidak punya padanan di `BlockDto`; dipakai pemotongan.
    Divider,

    /// `<ul>`/`<ol>`. Gaya labelnya ada di tiap [`InputRichBlockListItem`], bukan
    /// di sini — lihat catatannya.
    List { items: Vec<InputRichBlockListItem> },

    /// `<blockquote>` — kutipan inset Medium.
    Blockquote {
        blocks: Vec<InputRichBlock>,
        #[serde(skip_serializing_if = "Option::is_none")]
        credit: Option<RichText>,
    },

    /// `<aside>` — kutipan tarik Medium, yang di halaman dirender lebih besar
    /// dan menjorok. Itu `QuoteStyleDto::Pull`, bukan `Inset`.
    Pullquote {
        text: RichText,
        #[serde(skip_serializing_if = "Option::is_none")]
        credit: Option<RichText>,
    },

    /// `<tg-collage>` — deretan gambar berdampingan, yaitu `OUTSET_ROW`.
    Collage {
        blocks: Vec<InputRichBlock>,
        #[serde(skip_serializing_if = "Option::is_none")]
        caption: Option<RichBlockCaption>,
    },

    /// `<img>`. Telegram mengunduh `media` sendiri, jadi URL
    /// `miro.medium.com` diteruskan apa adanya.
    Photo {
        photo: InputMediaPhoto,
        #[serde(skip_serializing_if = "Option::is_none")]
        caption: Option<RichBlockCaption>,
    },
}

/// Satu butir list.
///
/// # `label_type` adalah `type`, dan ia berarti gaya penomoran
///
/// `InputRichBlockList` **tidak punya** flag `ordered`. Yang membedakan list
/// berurut dari list peluru adalah field `type` di tiap butir:
///
/// | nilai | artinya |
/// |---|---|
/// | *(dihilangkan)* | butir peluru |
/// | `"1"` | angka desimal |
/// | `"a"`/`"A"` | huruf |
/// | `"i"`/`"I"` | angka Romawi |
///
/// Namanya `label_type` di Rust karena `type` adalah kata kunci, dan
/// `#[serde(rename = "type")]` yang mengembalikannya ke kabel. `value` adalah
/// ordinal labelnya.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct InputRichBlockListItem {
    pub blocks: Vec<InputRichBlock>,
    #[serde(rename = "type", skip_serializing_if = "Option::is_none")]
    pub label_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value: Option<i32>,
}

impl InputRichBlockListItem {
    /// Butir peluru: tanpa `type`, tanpa `value`.
    #[must_use]
    pub fn bullet(blocks: Vec<InputRichBlock>) -> Self {
        Self {
            blocks,
            label_type: None,
            value: None,
        }
    }

    /// Butir list berurut. `ordinal` adalah nomor yang ditampilkan, dimulai
    /// dari 1.
    #[must_use]
    pub fn numbered(blocks: Vec<InputRichBlock>, ordinal: i32) -> Self {
        Self {
            blocks,
            label_type: Some("1".to_string()),
            value: Some(ordinal),
        }
    }
}

/// Teks di bawah sebuah blok media.
///
/// `credit` adalah `<cite>` — atribusi, bukan bagian dari caption itu sendiri.
/// Bot ini tidak pernah mengisinya: Medium tidak punya konsep itu, dan
/// memaksakan `alt` ke sana akan menampilkan teks alternatif sebagai atribusi.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RichBlockCaption {
    pub text: RichText,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub credit: Option<RichText>,
}

impl RichBlockCaption {
    #[must_use]
    pub fn new(text: RichText) -> Self {
        Self { text, credit: None }
    }
}

/// Foto yang akan diunduh Telegram.
///
/// `media` menerima **HTTP URL** selain `file_id`, dan itulah yang dipakai:
/// bot tidak pernah mengunggah apa pun, sehingga tidak perlu menyimpan state
/// `file_id` maupun menyalin gambar.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct InputMediaPhoto {
    #[serde(rename = "type")]
    pub kind: String,
    pub media: String,
}

impl InputMediaPhoto {
    #[must_use]
    pub fn from_url(url: impl Into<String>) -> Self {
        Self {
            kind: PHOTO.to_string(),
            media: url.into(),
        }
    }
}

/// Teks kaya, **union tanpa tag**.
///
/// Bentuknya di kabel adalah salah satu dari: string polos, array `RichText`,
/// atau objek ber-`type`. `untagged` biasanya bendera merah pada tipe yang
/// di-*deserialize*; di sini ia aman karena tipe ini **hanya pernah
/// diserialisasi**, dan pada serialisasi serde mencoba varian menurut urutan
/// deklarasinya — `String` hanya bisa jadi `Plain`, `Vec` hanya bisa jadi
/// `Parts`, map hanya bisa jadi `Node`. Tidak ada ambiguitas yang perlu
/// diselesaikan dengan menebak.
///
/// `Node` di-`Box` untuk memutus siklus ukuran: `RichNode::Bold` memuat
/// `RichText`, jadi tanpa kotak tipenya tak terhingga.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(untagged)]
pub enum RichText {
    Plain(String),
    Parts(Vec<RichText>),
    Node(Box<RichNode>),
}

impl RichText {
    #[must_use]
    pub fn plain(text: impl Into<String>) -> Self {
        Self::Plain(text.into())
    }

    #[must_use]
    pub fn node(node: RichNode) -> Self {
        Self::Node(Box::new(node))
    }

    /// Rangkaian inline jadi satu nilai, **tanpa membungkus yang tunggal**.
    ///
    /// `[x]` dan `x` sama artinya bagi Telegram — `RichText` union yang tidak
    /// bertag — jadi array berisi satu elemen hanya menambah bentuk tanpa
    /// menambah arti. Yang menulis array satu elemen di sini adalah pemetaan
    /// mekanis dari `Vec<InlineDto>`, dan itu bukan alasan yang cukup.
    #[must_use]
    pub fn parts(parts: Vec<Self>) -> Self {
        let mut parts = parts;
        match parts.len() {
            0 => Self::Plain(String::new()),
            1 => parts.pop().expect("panjangnya baru saja diperiksa"),
            _ => Self::Parts(parts),
        }
    }

    /// Teksnya tanpa format apa pun, urut sebagaimana ia muncul.
    ///
    /// Hanya yang **dirender** — tujuan sebuah tautan tidak ikut, karena ia
    /// tidak terlihat pembaca.
    #[must_use]
    pub fn flatten(&self) -> String {
        let mut out = String::new();
        self.write_rendered(&mut out);
        out
    }

    /// Panjang seluruh teks yang akan ikut terkirim, dalam **byte UTF-8**.
    ///
    /// Berbeda dari [`Self::flatten`] dalam dua hal, keduanya disengaja:
    ///
    /// - **Tujuan tautan ikut dihitung.** `href` adalah teks yang benar-benar
    ///   dikirim dan disimpan Telegram; satu artikel bertaut banyak bisa
    ///   membawa ribuan karakter yang tidak pernah muncul di [`Self::flatten`].
    ///   Mengabaikannya berarti memperkirakan pesan lebih pendek dari
    ///   kenyataannya, dan itulah satu-satunya arah kesalahan yang berakhir
    ///   dengan pesan ditolak.
    /// - **Satuannya byte.** Dokumen Bot API menyebut batasnya sebagai
    ///   *"32768 UTF-8 characters in the rich message text"*, dan "UTF-8
    ///   characters" bisa berarti tiga hal: byte, titik kode, atau satuan
    ///   UTF-16. Byte adalah **yang terbesar** dari ketiganya untuk teks apa pun
    ///   — satu emoji adalah 1 titik kode, 2 satuan UTF-16, dan 4 byte — jadi
    ///   menghitung byte adalah satu-satunya pilihan yang tidak bisa melewati
    ///   batas di bawah pembacaan mana pun.
    ///
    /// Harganya: artikel non-Latin dipotong lebih awal dari yang sebenarnya
    /// perlu, sekitar tiga kali untuk aksara Han. Itu arah yang benar untuk
    /// salah. Yang terjadi pada pembaca adalah tautan "Baca selengkapnya" yang
    /// muncul lebih cepat — terlihat, dan masih berguna. Arah sebaliknya adalah
    /// pesan yang ditolak, dan pembaca tidak menerima apa-apa.
    ///
    /// **Satuan ini sengaja berbeda** dari [`crate::update::MessageEntity`],
    /// yang `offset`-nya memang dihitung Telegram dalam satuan UTF-16 — itu
    /// konvensi yang berbeda untuk hal yang berbeda, dan menyamakannya adalah
    /// kesalahan.
    #[must_use]
    pub fn byte_len(&self) -> usize {
        match self {
            Self::Plain(text) => text.len(),
            Self::Parts(parts) => parts.iter().map(Self::byte_len).sum(),
            Self::Node(node) => node.byte_len(),
        }
    }

    fn write_rendered(&self, out: &mut String) {
        match self {
            Self::Plain(text) => out.push_str(text),
            Self::Parts(parts) => {
                for part in parts {
                    part.write_rendered(out);
                }
            }
            Self::Node(node) => node.write_rendered(out),
        }
    }
}

impl From<String> for RichText {
    fn from(text: String) -> Self {
        Self::Plain(text)
    }
}

impl From<&str> for RichText {
    fn from(text: &str) -> Self {
        Self::Plain(text.to_string())
    }
}

/// Sebuah node ber-`type`. Nama variannya yang snake_case jadi diskriminatornya.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RichNode {
    /// `<b>`
    Bold { text: RichText },
    /// `<i>`
    Italic { text: RichText },
    /// `<code>`
    Code { text: RichText },
    /// `<mark>` — penanda stabilo Medium.
    ///
    /// Sengaja bukan `spoiler`: `spoiler` **menyembunyikan** teksnya sampai
    /// disentuh, yang persis kebalikan dari apa yang dilakukan highlight
    /// Medium.
    Marked { text: RichText },
    /// `<a href>`.
    Url { text: RichText, url: String },
}

impl RichNode {
    fn byte_len(&self) -> usize {
        let (text, url) = match self {
            Self::Bold { text }
            | Self::Italic { text }
            | Self::Code { text }
            | Self::Marked { text } => (text, None),
            Self::Url { text, url } => (text, Some(url)),
        };

        text.byte_len() + url.map_or(0, String::len)
    }

    fn write_rendered(&self, out: &mut String) {
        let text = match self {
            Self::Bold { text }
            | Self::Italic { text }
            | Self::Code { text }
            | Self::Marked { text }
            | Self::Url { text, .. } => text,
        };
        text.write_rendered(out);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn serialized(message: &InputRichMessage) -> serde_json::Value {
        serde_json::to_value(message).expect("rich messages serialize")
    }

    /// Disetel `true`, dan itu keputusan yang bisa dibalik — lihat catatan
    /// modul. Test ini ada supaya membalikkannya terlihat, bukan diam-diam.
    #[test]
    fn a_rich_message_sets_skip_entity_detection() {
        let message = InputRichMessage::from_blocks(vec![InputRichBlock::Divider]);

        assert_eq!(message.skip_entity_detection, Some(true));
    }

    /// `InputRichMessage` menerima **tepat satu** dari `html`/`markdown`/`blocks`.
    /// Test ini yang menjaga agar tidak ada `"html": null` yang ikut terkirim —
    /// yang bisa dibaca Telegram sebagai dua mode sekaligus.
    #[test]
    fn a_rich_message_never_carries_an_html_or_markdown_key() {
        let value = serialized(&InputRichMessage::from_blocks(vec![
            InputRichBlock::Divider,
        ]));
        let object = value.as_object().expect("an object");

        assert!(object.contains_key("blocks"));
        assert!(!object.contains_key("html"));
        assert!(!object.contains_key("markdown"));
        assert_eq!(
            object.keys().collect::<Vec<_>>(),
            vec!["blocks", "skip_entity_detection"]
        );
    }

    /// Varian unit harus jadi `{"type":"divider"}` saja — tanpa `null` di
    /// mana pun. Ini satu-satunya blok yang tidak punya field.
    #[test]
    fn a_divider_is_only_its_type() {
        assert_eq!(
            serde_json::to_value(InputRichBlock::Divider).unwrap(),
            json!({ "type": "divider" })
        );
    }

    #[test]
    fn a_paragraph_is_a_tagged_object_with_its_text() {
        assert_eq!(
            serde_json::to_value(InputRichBlock::Paragraph {
                text: RichText::plain("apa saja")
            })
            .unwrap(),
            json!({ "type": "paragraph", "text": "apa saja" })
        );
    }

    /// `language: None` harus **hilang**, bukan `null`: Telegram memvalidasi
    /// nilai yang ada, dan `null` bukan nama bahasa.
    #[test]
    fn a_pre_without_a_language_omits_the_key() {
        let value = serde_json::to_value(InputRichBlock::Pre {
            text: RichText::plain("fn main() {}"),
            language: None,
        })
        .unwrap();

        assert_eq!(value, json!({ "type": "pre", "text": "fn main() {}" }));

        let with = serde_json::to_value(InputRichBlock::Pre {
            text: RichText::plain("fn main() {}"),
            language: Some("rust".to_string()),
        })
        .unwrap();

        assert_eq!(with["language"], json!("rust"));
    }

    #[test]
    fn a_heading_carries_its_size() {
        assert_eq!(
            serde_json::to_value(InputRichBlock::Heading {
                text: RichText::plain("Sol, Astra"),
                size: 3,
            })
            .unwrap(),
            json!({ "type": "heading", "text": "Sol, Astra", "size": 3 })
        );
    }

    /// Butir peluru tidak boleh punya `type` sama sekali — kehadirannya,
    /// sekalipun `null`, berarti "list bernomor dengan gaya tak dikenal".
    #[test]
    fn a_bullet_item_has_no_type_key() {
        let item = InputRichBlockListItem::bullet(vec![InputRichBlock::Paragraph {
            text: RichText::plain("satu"),
        }]);
        let value = serde_json::to_value(&item).unwrap();

        assert!(!value.as_object().unwrap().contains_key("type"));
        assert!(!value.as_object().unwrap().contains_key("value"));
    }

    /// Inilah satu-satunya yang membedakan list berurut dari list peluru, dan
    /// bidang inilah yang paling perlu dikonfirmasi lewat kirim sungguhan.
    #[test]
    fn a_numbered_item_declares_its_label_type_and_ordinal() {
        let item = InputRichBlockListItem::numbered(
            vec![InputRichBlock::Paragraph {
                text: RichText::plain("satu"),
            }],
            1,
        );

        assert_eq!(
            serde_json::to_value(&item).unwrap(),
            json!({
                "type": "1",
                "value": 1,
                "blocks": [{ "type": "paragraph", "text": "satu" }],
            })
        );
    }

    #[test]
    fn a_photo_is_an_input_media_photo_with_its_type() {
        assert_eq!(
            serde_json::to_value(InputRichBlock::Photo {
                photo: InputMediaPhoto::from_url("https://miro.medium.com/v2/1*a.png"),
                caption: None,
            })
            .unwrap(),
            json!({
                "type": "photo",
                "photo": {
                    "type": "photo",
                    "media": "https://miro.medium.com/v2/1*a.png",
                },
            })
        );
    }

    #[test]
    fn a_caption_is_an_object_not_a_bare_string() {
        assert_eq!(
            serde_json::to_value(RichBlockCaption::new(RichText::plain("keterangan"))).unwrap(),
            json!({ "text": "keterangan" })
        );
    }

    /// Tiga bentuk `RichText` di kabel, masing-masing jadi JSON yang berbeda
    /// jenisnya. Inilah alasan `untagged` aman di sini.
    #[test]
    fn rich_text_serializes_as_a_string_an_array_or_an_object() {
        assert_eq!(
            serde_json::to_value(RichText::plain("x")).unwrap(),
            json!("x")
        );

        assert_eq!(
            serde_json::to_value(RichText::Parts(vec![
                RichText::plain("a"),
                RichText::plain("b"),
            ]))
            .unwrap(),
            json!(["a", "b"])
        );

        assert_eq!(
            serde_json::to_value(RichText::node(RichNode::Bold {
                text: RichText::plain("x")
            }))
            .unwrap(),
            json!({ "type": "bold", "text": "x" })
        );
    }

    /// `Url` satu-satunya node dengan dua field, dan urutannya tidak penting
    /// selama keduanya ada.
    #[test]
    fn a_url_node_carries_both_its_label_and_its_destination() {
        assert_eq!(
            serde_json::to_value(RichText::node(RichNode::Url {
                text: RichText::plain("Medium"),
                url: "https://medium.com".to_string(),
            }))
            .unwrap(),
            json!({ "type": "url", "text": "Medium", "url": "https://medium.com" })
        );
    }

    /// Format bersarang harus tetap satu pohon — `bold` yang memuat `italic`
    /// tidak boleh jadi dua node bersaudara.
    #[test]
    fn formatting_nodes_nest_rather_than_sit_side_by_side() {
        assert_eq!(
            serde_json::to_value(RichText::node(RichNode::Bold {
                text: RichText::node(RichNode::Italic {
                    text: RichText::plain("keduanya"),
                }),
            }))
            .unwrap(),
            json!({
                "type": "bold",
                "text": { "type": "italic", "text": "keduanya" },
            })
        );
    }

    /// Perataan mengikuti urutan penyisipan, termasuk ke dalam array dan node.
    #[test]
    fn flattening_walks_the_whole_tree_in_order() {
        let text = RichText::Parts(vec![
            RichText::plain("a"),
            RichText::node(RichNode::Bold {
                text: RichText::Parts(vec![RichText::plain("b"), RichText::plain("c")]),
            }),
            RichText::plain("d"),
        ]);

        assert_eq!(text.flatten(), "abcd");
    }

    /// Ketiga satuan panjang berbeda begitu ada aksara di luar ASCII, dan yang
    /// dipakai adalah yang **terbesar** — lihat [`RichText::byte_len`].
    ///
    /// Test ini gagal kalau satuannya berubah, dan itu memang gunanya: batasnya
    /// dinyatakan dokumen Bot API sebagai "32768 UTF-8 characters", dan byte
    /// adalah satu-satunya pembacaan yang tidak bisa melewati batas di bawah dua
    /// pembacaan lainnya.
    #[test]
    fn length_is_counted_in_bytes_because_that_is_the_largest_of_the_three() {
        let text = RichText::plain("é☃😀");

        assert_eq!(text.byte_len(), 9, "2 + 3 + 4 byte");
        assert_eq!(text.flatten().chars().count(), 3, "tiga titik kode");
        assert_eq!(
            text.flatten().encode_utf16().count(),
            4,
            "1 + 1 + 2 satuan UTF-16 — lebih kecil dari byte, dan itu sebabnya byte dipakai"
        );
    }

    /// Panjang menjumlah seluruh potongan, termasuk yang bersarang di dalam
    /// node.
    #[test]
    fn length_sums_every_fragment_of_a_nested_tree() {
        let text = RichText::Parts(vec![
            RichText::plain("ab"),
            RichText::node(RichNode::Bold {
                text: RichText::plain("cd"),
            }),
        ]);

        assert_eq!(text.byte_len(), 4);
    }

    /// Tujuan tautan ikut dihitung meski ia tidak pernah dirender — inilah
    /// satu-satunya perbedaan [`RichText::flatten`] dan [`RichText::byte_len`],
    /// dan arah kesalahannya disengaja: lebih baik memotong terlalu awal
    /// daripada mengirim pesan yang ditolak Telegram.
    #[test]
    fn length_counts_a_links_destination_even_though_nothing_renders_it() {
        let text = RichText::node(RichNode::Url {
            text: RichText::plain("Medium"),
            url: "https://medium.com".to_string(),
        });

        assert_eq!(text.flatten(), "Medium");
        assert_eq!(text.byte_len(), "Medium".len() + "https://medium.com".len());
    }
}
