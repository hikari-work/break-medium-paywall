//! Anggaran rich message, dan pemotongan di batas blok.
//!
//! # Kenapa memotong, dan kenapa di sini
//!
//! Telegram menolak rich message yang melewati batasnya — bukan memotongnya
//! sendiri, dan bukan mengirim sebagian. Artikel Medium panjangnya tak
//! terbatas, jadi memotong adalah sesuatu yang harus dilakukan bot ini, dan
//! satu-satunya pertanyaan yang tersisa adalah di mana potongannya jatuh.
//!
//! Jawabannya: **di batas blok teratas**, dan tidak pernah di tengah. Memotong
//! di tengah sebuah `RichText` berarti membelah pohon teks berformat — `<b>`
//! yang kehilangan penutupnya, tautan yang teksnya hilang separuh. Memotong
//! di antara blok hanya kehilangan blok.
//!
//! Aturannya bukan "lewati yang tidak muat lalu lanjutkan": blok pertama yang
//! tidak muat **mengakhiri** pesan. Melanjutkan setelahnya akan membuat
//! artikel yang bagian tengahnya hilang tanpa jejak — jauh lebih membingungkan
//! daripada artikel yang berhenti lebih awal dan mengatakannya.
//!
//! # Angka-angkanya
//!
//! Empat dari lima batas rich message berlaku di sini. Yang kelima, 20 kolom
//! tabel, tidak: pipeline Medium di repo ini tidak pernah menghasilkan tabel
//! (`medium-doc/src/parse.rs:768` membuang tipe yang tidak dikenal), jadi
//! [`InputRichBlock`] bahkan tidak punya variannya.
//!
//! # Yang dihitung, dan satuan yang dipakai
//!
//! Setiap string yang ikut terkirim dihitung — termasuk `language` pada `pre`,
//! label list, dan **tujuan setiap tautan**. Angka ordinal list tidak, karena
//! ia terkirim sebagai bilangan, bukan teks.
//!
//! Aturan itu dipilih karena arah kesalahannya jelas: menghitung lebih berarti
//! memotong sedikit terlalu awal — satu blok hilang, dan tautan penutup tetap
//! membawa pembaca ke artikel utuhnya. Menghitung kurang berarti Telegram
//! menolak seluruh pesan, dan pembaca tidak dapat apa-apa.
//!
//! # Satuannya byte, dan itu pilihan yang sadar
//!
//! Dokumen Bot API menyebut batasnya sebagai *"32768 UTF-8 characters in the
//! rich message text"*. "UTF-8 characters" bisa berarti tiga hal — byte, titik
//! kode, atau satuan UTF-16 — dan **byte adalah yang terbesar** dari ketiganya.
//! Karena itu byte yang dipakai: apa pun yang sebenarnya dimaksud Telegram,
//! angka yang dihitung di sini tidak pernah lebih kecil darinya, jadi pesan
//! tidak bisa melewati batas di bawah pembacaan mana pun.
//!
//! Harganya nyata: artikel beraksara non-Latin dipotong lebih awal dari yang
//! sebenarnya perlu — sekitar tiga kali lebih awal untuk aksara Han yang
//! memakan 3 byte per titik kode. Yang dibeli dengan harga itu adalah pesan
//! yang tidak pernah ditolak, dan tautan penutup yang tetap membawa pembaca ke
//! sisa artikelnya.
//!
//! Satuan ini **berbeda** dari [`crate::update::MessageEntity`], yang offsetnya
//! memang UTF-16 karena begitulah Bot API mendefinisikannya. Keduanya sengaja
//! dibiarkan berbeda; menyamakannya berarti salah di salah satu tempat.

use super::model::{InputRichBlock, InputRichBlockListItem, RichBlockCaption, RichText};

/// Batas panjang teks sebuah rich message, dalam **byte UTF-8**.
pub const MAX_BYTES: usize = 32_768;

/// Batas jumlah blok, **termasuk yang bersarang**: butir list dan isinya,
/// blok di dalam kutipan, dan foto di dalam kolase masing-masing dihitung.
pub const MAX_BLOCKS: usize = 500;

/// Batas jumlah media. Satu kolase berisi N foto dihitung N.
pub const MAX_MEDIA: usize = 50;

/// Batas kedalaman bersarang. Blok teratas berkedalaman 1.
pub const MAX_DEPTH: usize = 16;

/// Berapa banyak pesan yang dipakai sebuah rangkaian blok.
///
/// `depth` adalah **kedalaman maksimum**, bukan jumlah — blok bersaudara
/// bertumpuk satu sama lain, bukan menjumlah.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Cost {
    /// Byte UTF-8, bukan titik kode dan bukan satuan UTF-16 — lihat catatan
    /// modul.
    pub bytes: usize,
    pub blocks: usize,
    pub media: usize,
    pub depth: usize,
}

impl Cost {
    /// Anggaran sebuah rich message — dipakai sekaligus sebagai batas.
    #[must_use]
    pub fn ceiling() -> Self {
        Self {
            bytes: MAX_BYTES,
            blocks: MAX_BLOCKS,
            media: MAX_MEDIA,
            depth: MAX_DEPTH,
        }
    }

    /// Berapa yang dipakai sederet blok yang duduk bersebelahan — blok teratas
    /// sebuah pesan, misalnya.
    #[must_use]
    pub fn of_blocks(blocks: &[InputRichBlock]) -> Self {
        blocks.iter().fold(Self::default(), |cost, block| {
            cost.plus(Self::of_block(block))
        })
    }

    /// Berapa yang dipakai satu blok, servernya sendiri termasuk.
    #[must_use]
    pub fn of_block(block: &InputRichBlock) -> Self {
        match block {
            InputRichBlock::Paragraph { text }
            | InputRichBlock::Heading { text, .. }
            | InputRichBlock::Footer { text } => Self::text(text),

            InputRichBlock::Pre { text, language } => {
                Self::text(text).plus(Self::bytes(language.as_deref().unwrap_or_default()))
            }

            InputRichBlock::Divider => Self::one_block(),

            InputRichBlock::Pullquote { text, credit } => {
                Self::text(text).plus(Self::optional_text(credit))
            }

            InputRichBlock::List { items } => items.iter().fold(Self::one_block(), |cost, item| {
                cost.wrapping(Self::of_list_item(item))
            }),

            InputRichBlock::Blockquote { blocks, credit } => {
                Self::of_children(blocks).plus(Self::optional_text(credit))
            }

            InputRichBlock::Collage { blocks, caption } => {
                Self::of_children(blocks).plus(Self::optional_caption(caption))
            }

            InputRichBlock::Photo { caption, .. } => Self::one_block()
                .media(1)
                .plus(Self::optional_caption(caption)),
        }
    }

    /// Blok yang membungkus blok lain: dirinya, plus isinya, satu tingkat lebih
    /// dalam.
    fn of_children(blocks: &[InputRichBlock]) -> Self {
        blocks.iter().fold(Self::one_block(), |cost, block| {
            cost.wrapping(Self::of_block(block))
        })
    }

    fn of_list_item(item: &InputRichBlockListItem) -> Self {
        // Butir list adalah blok tersendiri bagi Telegram, dan labelnya ikut
        // terkirim sebagai teks.
        item.blocks
            .iter()
            .fold(Self::one_block(), |cost, block| {
                cost.wrapping(Self::of_block(block))
            })
            .plus(Self::bytes(item.label_type.as_deref().unwrap_or_default()))
    }

    fn of_caption(caption: &RichBlockCaption) -> Self {
        Self::inline_text(&caption.text).plus(Self::optional_text(&caption.credit))
    }

    fn optional_caption(caption: &Option<RichBlockCaption>) -> Self {
        caption.as_ref().map_or(Self::default(), Self::of_caption)
    }

    fn optional_text(text: &Option<RichText>) -> Self {
        text.as_ref().map_or(Self::default(), Self::inline_text)
    }

    /// Blok berisi teks saja.
    fn text(text: &RichText) -> Self {
        Self::one_block().plus(Self::inline_text(text))
    }

    /// Teks yang **bukan** blok: caption dan credit menempel pada blok lain,
    /// jadi mereka tidak menambah hitungan blok.
    fn inline_text(text: &RichText) -> Self {
        Self::count(text.byte_len())
    }

    /// Teks yang bukan [`RichText`] — label list, nama bahasa.
    fn bytes(text: &str) -> Self {
        Self::count(text.len())
    }

    /// Panjang yang sudah dalam satuan byte UTF-8 — lihat [`RichText::byte_len`].
    fn count(bytes: usize) -> Self {
        Self {
            bytes,
            ..Self::default()
        }
    }

    fn one_block() -> Self {
        Self {
            blocks: 1,
            depth: 1,
            ..Self::default()
        }
    }

    fn media(self, count: usize) -> Self {
        Self {
            media: self.media.saturating_add(count),
            ..self
        }
    }

    /// Dua pemakaian yang duduk **bersebelahan** dan harus muat berdua.
    ///
    /// Kedalaman diambil yang terdalam, bukan dijumlah: blok bersaudara tidak
    /// saling menumpuk. Membedakan ini dari [`Self::wrapping`] adalah hal yang
    /// paling mudah salah di berkas ini — pernah salah, dan kedalamannya bocor
    /// satu tingkat per blok sampai artikel biasa pun tampak terlalu dalam.
    fn plus(self, other: Self) -> Self {
        Self {
            bytes: self.bytes.saturating_add(other.bytes),
            blocks: self.blocks.saturating_add(other.blocks),
            media: self.media.saturating_add(other.media),
            depth: self.depth.max(other.depth),
        }
    }

    /// `child` berada **di dalam** `self`, jadi kedalamannya bertambah satu.
    fn wrapping(self, child: Self) -> Self {
        Self {
            bytes: self.bytes.saturating_add(child.bytes),
            blocks: self.blocks.saturating_add(child.blocks),
            media: self.media.saturating_add(child.media),
            depth: self.depth.max(child.depth.saturating_add(1)),
        }
    }

    /// Sisa anggaran setelah [`Self::plus`] — dipakai menyisihkan ruang untuk
    /// blok penutup sebelum satu blok pun diukur.
    ///
    /// Kedalaman **tidak** dikurangi meski yang lain dikurangi, karena ia
    /// maksimum dan blok penutup duduk bersebelahan dengan blok isi, bukan di
    /// dalamnya. Menguranginya akan memotong artikel yang sebenarnya muat:
    /// satu tingkat anggaran hilang untuk sesuatu yang tidak memakainya.
    ///
    /// Mengumpul di nol, bukan mengunder-alir. Kalau blok penutupnya sendiri
    /// lebih besar dari anggaran, hasilnya nol dan tidak ada blok isi yang
    /// muat — lihat catatan [`fit`].
    fn minus(self, other: Self) -> Self {
        Self {
            bytes: self.bytes.saturating_sub(other.bytes),
            blocks: self.blocks.saturating_sub(other.blocks),
            media: self.media.saturating_sub(other.media),
            depth: self.depth,
        }
    }

    /// Apakah pemakaian ini masih di dalam `cap` untuk keempat batasnya
    /// sekaligus.
    #[must_use]
    pub fn fits_within(self, cap: Self) -> bool {
        self.bytes <= cap.bytes
            && self.blocks <= cap.blocks
            && self.media <= cap.media
            && self.depth <= cap.depth
    }
}

/// Hasil [`fit`].
#[derive(Debug, Clone, PartialEq)]
pub struct Budgeted {
    /// Blok yang akan dikirim, sudah termasuk `closing` kalau dipotong.
    pub blocks: Vec<InputRichBlock>,

    /// Apakah ada blok yang dibuang. Dipakai pemanggil untuk mencatat — pesan
    /// yang dipotong diam-diam adalah pesan yang tampak rusak.
    pub truncated: bool,
}

/// Memotong `blocks` supaya muat, dengan `closing` sebagai penutupnya.
///
/// Ruang untuk `closing` disisihkan **sebelum** blok pertama diukur, bukan
/// ditambahkan setelahnya. Urutannya penting: kalau tidak, artikel yang
/// pas-pasan akan ditutup blok yang membuatnya melewati batas, dan Telegram
/// menolak keseluruhannya.
///
/// `closing` hanya ikut kalau memang ada yang dipotong. Artikel yang muat utuh
/// tidak boleh berpura-pura terpotong.
///
/// # Jaminan
///
/// Hasilnya muat di [`Cost::ceiling`] **selama `closing` sendiri muat**. Kalau
/// tidak — blok penutup yang lebih besar dari batas pesan — hasilnya hanya
/// `closing`, dan tidak ada susunan blok lain yang bisa memperbaikinya.
#[must_use]
pub fn fit(blocks: Vec<InputRichBlock>, closing: InputRichBlock) -> Budgeted {
    let ceiling = Cost::ceiling().minus(Cost::of_block(&closing));
    let mut kept: Vec<InputRichBlock> = Vec::new();
    let mut used = Cost::default();

    for block in blocks {
        let cost = Cost::of_block(&block);

        if !used.plus(cost).fits_within(ceiling) {
            kept.push(closing);
            return Budgeted {
                blocks: kept,
                truncated: true,
            };
        }

        used = used.plus(cost);
        kept.push(block);
    }

    Budgeted {
        blocks: kept,
        truncated: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rich::model::{InputMediaPhoto, RichNode};

    fn paragraph(text: &str) -> InputRichBlock {
        InputRichBlock::Paragraph {
            text: RichText::plain(text),
        }
    }

    fn photo(url: &str) -> InputRichBlock {
        InputRichBlock::Photo {
            photo: InputMediaPhoto::from_url(url),
            caption: None,
        }
    }

    /// Blok penutup yang bentuknya sama dengan yang dipakai `rich::truncated_notice`.
    fn closing() -> InputRichBlock {
        InputRichBlock::Paragraph {
            text: RichText::Parts(vec![
                RichText::plain("… "),
                RichText::node(RichNode::Url {
                    text: RichText::plain("Baca selengkapnya"),
                    url: "https://medium.piyann.my.id/p/e6047997b667".to_string(),
                }),
            ]),
        }
    }

    #[test]
    fn a_block_costs_its_text_plus_itself() {
        let cost = Cost::of_block(&paragraph("12345"));

        assert_eq!(
            cost,
            Cost {
                bytes: 5,
                blocks: 1,
                media: 0,
                depth: 1,
            }
        );
    }

    /// Tujuan tautan tidak pernah dirender, tapi ia tetap terkirim — dan satu
    /// artikel bisa memuat puluhan tautan.
    #[test]
    fn a_link_costs_its_destination_as_well_as_its_label() {
        let block = paragraph("");
        let cost = Cost::of_block(&InputRichBlock::Paragraph {
            text: RichText::node(RichNode::Url {
                text: RichText::plain("ab"),
                url: "https://medium.com/p/abcdef".to_string(),
            }),
        });

        assert_eq!(cost.bytes, 2 + "https://medium.com/p/abcdef".len());
        assert_eq!(Cost::of_block(&block).bytes, 0);
    }

    /// Butir list adalah blok bagi Telegram, dan labelnya ikut terkirim.
    #[test]
    fn list_items_and_their_labels_count_as_blocks() {
        let list = InputRichBlock::List {
            items: vec![
                InputRichBlockListItem::numbered(vec![paragraph("satu")], 1),
                InputRichBlockListItem::numbered(vec![paragraph("dua")], 2),
            ],
        };
        let cost = Cost::of_block(&list);

        assert_eq!(cost.blocks, 1 + 2 * 2, "list, dua butir, dua paragraf");
        assert_eq!(
            cost.bytes,
            "satu".len() + "dua".len() + 1 + 1,
            "teks plus label `1` dan `2`"
        );
        assert_eq!(cost.depth, 3, "list → butir → paragraf");
    }

    #[test]
    fn photos_count_toward_the_media_limit_and_text_does_not() {
        assert_eq!(Cost::of_block(&photo("https://x/1.png")).media, 1);
        assert_eq!(Cost::of_block(&paragraph("apa saja")).media, 0);
    }

    /// Kolase adalah satu blok berisi N foto — dan fotonya tetap dihitung
    /// sebagai media, bukan sebagai blok baru.
    #[test]
    fn a_collage_wraps_its_photos_without_hiding_them_from_the_media_count() {
        let collage = InputRichBlock::Collage {
            blocks: vec![photo("https://x/1.png"), photo("https://x/2.png")],
            caption: Some(RichBlockCaption::new(RichText::plain("empat"))),
        };
        let cost = Cost::of_block(&collage);

        assert_eq!(cost.media, 2);
        assert_eq!(cost.blocks, 3, "kolase plus dua foto");
        assert_eq!(cost.bytes, 5, "hanya caption yang berteks");
    }

    #[test]
    fn an_article_that_fits_keeps_every_block_and_gains_no_closing_block() {
        let result = fit(vec![paragraph("satu"), paragraph("dua")], closing());

        assert!(!result.truncated);
        assert_eq!(result.blocks, vec![paragraph("satu"), paragraph("dua")]);
    }

    #[test]
    fn an_empty_article_is_not_an_article_that_was_cut() {
        let result = fit(Vec::new(), closing());

        assert!(!result.truncated);
        assert!(result.blocks.is_empty());
    }

    /// Aturan intinya: blok pertama yang tidak muat **mengakhiri** pesan.
    /// Blok sesudahnya tidak boleh muncul walau ia muat sendiri — itu akan
    /// membuat bagian tengah artikel hilang tanpa jejak.
    #[test]
    fn the_first_block_that_does_not_fit_ends_the_message() {
        let big = "x".repeat(MAX_BYTES);
        let result = fit(
            vec![paragraph("kecil"), paragraph(&big), paragraph("muat")],
            closing(),
        );

        assert!(result.truncated);
        assert_eq!(
            result.blocks,
            vec![paragraph("kecil"), closing()],
            "`muat` dibuang walau ia sendirian akan muat"
        );
    }

    /// Ruang untuk blok penutup diambil lebih dulu. Dua paragraf di sini muat
    /// satu-satu, dan muat berdua **tanpa** blok penutup — yang membuatnya
    /// tidak muat adalah penutupnya sendiri.
    #[test]
    fn room_for_the_closing_block_is_reserved_before_anything_is_measured() {
        let half = "x".repeat(MAX_BYTES / 2 - 4);
        assert!(
            half.len() * 2 + "… Baca selengkapnya".len() + 40 > MAX_BYTES,
            "artikel ini harus pas-pasan supaya testnya berarti"
        );

        let result = fit(vec![paragraph(&half), paragraph(&half)], closing());

        assert!(
            result.truncated,
            "tanpa sisihan, penutupnya yang bikin lewat"
        );
        assert_eq!(result.blocks, vec![paragraph(&half), closing()]);
    }

    /// Kalau `closing` sendiri muat, hasilnya selalu muat — termasuk blok
    /// penutupnya. Inilah janji yang membuat pemotongan ini aman.
    #[test]
    fn a_truncated_message_still_fits_by_construction() {
        let long = "x".repeat(MAX_BYTES / 3);
        let result = fit(
            vec![
                paragraph(&long),
                paragraph(&long),
                paragraph(&long),
                paragraph(&long),
            ],
            closing(),
        );

        assert!(result.truncated);

        let total = result.blocks.iter().fold(Cost::default(), |cost, block| {
            cost.plus(Cost::of_block(block))
        });

        assert!(total.fits_within(Cost::ceiling()), "{total:?} harus muat");
    }

    /// Satu blok yang sendirian melebihi batas tidak menyisakan apa pun selain
    /// penutupnya.
    #[test]
    fn a_single_oversized_block_leaves_only_the_closing_block() {
        let result = fit(vec![paragraph(&"x".repeat(MAX_BYTES + 1))], closing());

        assert!(result.truncated);
        assert_eq!(result.blocks, vec![closing()]);
    }

    /// Butir list ikut dihitung, jadi list yang terlalu panjang dibuang
    /// seluruhnya — bukan dipangkas di tengah, yang akan mengubah arti
    /// penomoran Medium.
    #[test]
    fn a_list_with_too_many_items_is_dropped_whole() {
        let items = |count: usize| {
            (1..=count)
                .map(|n| InputRichBlockListItem::numbered(vec![paragraph("butir")], n as i32))
                .collect::<Vec<_>>()
        };

        let fits = fit(vec![InputRichBlock::List { items: items(100) }], closing());
        assert!(!fits.truncated, "100 butir = 201 blok, masih muat");

        let overflows = fit(
            vec![InputRichBlock::List {
                items: items(MAX_BLOCKS),
            }],
            closing(),
        );
        assert!(overflows.truncated);
        assert_eq!(overflows.blocks, vec![closing()]);
    }

    #[test]
    fn photos_beyond_the_media_limit_end_the_message() {
        let photos: Vec<_> = (0..=MAX_MEDIA)
            .map(|n| photo(&format!("https://x/{n}.png")))
            .collect();

        let result = fit(photos, closing());

        assert!(result.truncated);
        assert_eq!(result.blocks.len(), MAX_MEDIA + 1, "50 foto plus penutup");
        assert_eq!(result.blocks.last(), Some(&closing()));
    }

    /// Bersarang lebih dalam dari 16 tingkat ditolak Telegram. Tidak ada
    /// pipeline Medium yang menghasilkan itu hari ini — test ini menjaga
    /// anggarannya tetap dihitung seandainya ada.
    ///
    /// Perhatikan hitungannya: paragraf di dalam `n` kutipan bersarang berada
    /// di kedalaman `n + 1`, karena blok teratas sendiri sudah kedalaman 1.
    #[test]
    fn nesting_deeper_than_the_limit_is_dropped() {
        let nested = |depth: usize| {
            (0..depth).fold(paragraph("inti"), |inner, _| InputRichBlock::Blockquote {
                blocks: vec![inner],
                credit: None,
            })
        };

        assert_eq!(Cost::of_block(&nested(3)).depth, 4);
        assert!(!fit(vec![nested(MAX_DEPTH - 1)], closing()).truncated);
        assert!(fit(vec![nested(MAX_DEPTH)], closing()).truncated);
    }

    /// Kedalaman adalah maksimum, bukan jumlah: seratus blok bersaudara
    /// masing-masing sedalam tiga tingkat tetap sedalam tiga tingkat. Kalau
    /// [`Cost::plus`] sampai menjumlahkannya lagi, test ini yang jatuh lebih
    /// dulu daripada artikel sungguhan.
    #[test]
    fn sibling_blocks_do_not_accumulate_depth() {
        let nested = |depth: usize| {
            (0..depth).fold(paragraph("inti"), |inner, _| InputRichBlock::Blockquote {
                blocks: vec![inner],
                credit: None,
            })
        };
        let siblings: Vec<_> = (0..100).map(|_| nested(3)).collect();

        let result = fit(siblings, closing());

        assert!(!result.truncated);
        assert_eq!(result.blocks.len(), 100);
    }

    /// Penutup yang lebih besar dari batas pesan tidak bisa diselamatkan
    /// susunan blok apa pun — hasilnya penutup itu sendiri.
    #[test]
    fn a_closing_block_larger_than_the_message_survives_alone() {
        let huge = InputRichBlock::Paragraph {
            text: RichText::plain("x".repeat(MAX_BYTES * 2)),
        };

        let result = fit(vec![paragraph("satu")], huge.clone());

        assert!(result.truncated);
        assert_eq!(result.blocks, vec![huge]);
    }
}
