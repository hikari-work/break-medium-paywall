//! `BlockDto` → rich message Telegram.
//!
//! | Berkas | Isinya |
//! |---|---|
//! | [`model`] | Bentuk wire Bot API — tipe keluaran dan penelusuran pohon teksnya |
//! | [`budget`] | Batas rich message, biaya tiap blok, dan pemotongan |
//! | di sini | Konversi `BlockDto`/`InlineDto` jadi blok, plus kepala dan ekor pesan |
//!
//! Urutannya disengaja: `model` tidak tahu apa-apa soal Medium, `budget` tidak
//! tahu apa-apa soal JSON, dan berkas ini tidak tahu apa-apa soal HTTP.
//!
//! # Yang dirender, dan dalam urutan apa
//!
//! `PostDto` memisahkan `meta` dari `blocks`, dan pesan Telegram tidak punya
//! tempat lain untuk metadatanya — jadi kepala pesan dibangun di sini, bukan
//! diserahkan pemanggil:
//!
//! | # | Blok | Sumber | Halaman menampilkannya? |
//! |---|---|---|---|
//! | 1 | `photo` gambar pratinjau | `meta.preview_image_url` | ya, di atas judul |
//! | 2 | `heading size=1` judul | `meta.title` | ya, `<h1>` |
//! | 3 | `paragraph` miring subjudul | `meta.subtitle` | ya, `<h2>` |
//! | 4 | `footer` byline | `meta.creator`, `reading_time_minutes`, tanggal | ya, kartu penulis |
//! | 5 | isi artikel | `blocks` | ya |
//! | 6 | `footer` tag | `meta.tags` | ya, chip `#tag` |
//!
//! Urutannya mengikuti `medium-render/templates/post.html`, bukan selera: gambar
//! pratinjau memang di atas `<h1>` di sana, dan memindahkannya ke bawah berarti
//! pesan ini dan halaman itu tidak lagi menampilkan hal yang sama.
//!
//! # Divergensi yang disengaja
//!
//! "Sama persis" bukan berarti setiap elemen halaman punya padanannya, dan
//! berpura-pura sebaliknya akan menyembunyikan yang sebenarnya terjadi:
//!
//! | Hilang | Kenapa |
//! |---|---|
//! | tombol **Follow**, kartu penulis, avatar | Telegram tidak punya kartu penulis; namanya tetap ada, di byline |
//! | tautan **"Go to the original"** | `meta.medium_url` sudah jadi identitas artikel; menaruhnya di kepala pesan mengundang pembaca keluar dari bot ini |
//! | **"Free: Yes/No"** | itu `is_locked` yang dibalik oleh warisan; pembaca di Telegram tidak bisa berbuat apa-apa dengan tahuinya |
//! | `drop_cap`, `HeadingSpacing`, `ParagraphMargin` | kelas Tailwind; tidak ada padanannya |
//! | `BlockDto::Heading`'s `id` | jangkar HTML; `RichText` tidak punya jangkar |
//! | `Link`'s `rel`, `title`, `new_tab` | tidak ada padanannya |
//! | `Embed`'s `site` dan `thumbnail_url` | Telegram tidak punya blok kartu — lihat [`embed_text`] |
//! | `Iframe`'s `width`/`height` | tidak ada blok iframe — lihat [`iframe_text`] |
//!
//! | pesan yang seluruh isinya kosong | Telegram menolaknya — lihat [`empty_notice`] |
//!
//! # `Embed` dan `Iframe` adalah divergensi yang nyata
//!
//! Keduanya bukan halaman Medium, melainkan hal yang **disematkan ke dalamnya**:
//! kartu tautan, dan bingkai milik situs lain. Rich message Telegram tidak punya
//! satu pun dari keduanya. Yang bisa dilakukan hanyalah mempertahankan
//! informasinya — URL, judul, ringkasan — dalam bentuk yang Telegram memang
//! punya, dan mengatakannya di sini alih-alih membiarkannya tampak seperti
//! konversi yang mulus.

pub mod budget;
pub mod model;

use budget::{Budgeted, fit};
use freedium_dto::{BlockDto, InlineDto, MetaDto, PostDto, QuoteStyleDto};
use model::{
    InputMediaPhoto, InputRichBlock, InputRichBlockListItem, InputRichMessage, RichBlockCaption,
    RichNode, RichText,
};

/// Basis tautan `@mention` Medium. Satu-satunya URL yang bisa dibentuk mention,
/// sama seperti yang dipakai `medium-render` dan `dto.rs`.
const MENTION_BASE: &str = "https://medium.com/u/";

/// Basis profil penulis. [`freedium_dto::CreatorDto`] sengaja tidak membawa URL
/// profil karena ia bisa dibentuk dari `username` — lihat catatan tipe itu.
const PROFILE_BASE: &str = "https://medium.com/@";

/// Artikel yang sudah jadi pesan, beserta apa yang terjadi padanya.
#[derive(Debug, Clone, PartialEq)]
pub struct Rendered {
    pub message: InputRichMessage,

    /// Apakah isinya dipotong supaya muat. Pemanggil mencatatnya, karena pesan
    /// yang berhenti di tengah tanpa ada yang tahu adalah pesan yang tampak
    /// rusak.
    pub truncated: bool,
}

/// Mengubah satu artikel jadi pesan siap kirim.
///
/// `base_url` adalah alamat deployment ini, dipakai hanya untuk tautan penutup
/// saat artikelnya dipotong — `{base_url}/p/{post_id}`, rute yang sama dengan
/// halaman artikelnya.
///
/// Hasilnya **tidak pernah** berupa pesan tanpa blok; lihat [`empty_notice`].
#[must_use]
pub fn rich_message(post: &PostDto, base_url: &str) -> Rendered {
    rich_message_with_photos(post, base_url, usize::MAX)
}

/// Seperti [`rich_message`], tapi berhenti memberi foto setelah `max_photos`.
///
/// # Kenapa batas foto ada sama sekali
///
/// Jalur biasa tidak membutuhkannya: `sendRichMessage` ke sebuah obrolan
/// menerima berapa pun foto yang muat di anggaran. Hasil **inline** tidak.
/// Artikel dengan satu foto lolos sebagai panggilan tamu; artikel dengan tujuh
/// ditolak `400 Bad Request: invalid inline message content specified` —
/// padahal isi yang sama persis terkirim utuh di DM, 47 blok dan tujuh foto.
///
/// Yang membedakan keduanya belum bisa dipastikan dari sini, dan itu sebabnya
/// batasnya sebuah **parameter** alih-alih angka yang ditanam: pemanggil yang
/// menurunkannya bertahap sampai Telegram menerima, lalu mencatat di mana
/// berhentinya.
///
/// # Yang dikorbankan fotonya, bukan teksnya
///
/// Blok gambar yang lewat batas **dibuang**, dan sisa artikelnya tetap utuh.
/// Membiarkan [`fit`] yang memotong akan membuang justru bagian terbesar
/// artikelnya, karena pemotongan berhenti di blok pertama yang lewat — dan
/// kehilangan seluruh sisa teks jauh lebih merugikan pembaca daripada
/// kehilangan gambar.
///
/// Satu `ImageRow` ikut seluruhnya atau tidak sama sekali, dengan alasan yang
/// sama: kolase separuh adalah gambar yang berubah arti, bukan gambar yang
/// kurang satu.
///
/// `max_photos` menghitung **semua** foto, termasuk gambar pratinjau di atas
/// judul. Jadi `1` menyisakan tepat gambar pratinjaunya, dan `0` membuang
/// fotonya sama sekali.
#[must_use]
pub fn rich_message_with_photos(post: &PostDto, base_url: &str, max_photos: usize) -> Rendered {
    let mut photos = max_photos;

    let mut blocks = header(&post.meta, &mut photos);
    blocks.extend(post.blocks.iter().filter_map(|dto| block(dto, &mut photos)));
    blocks.extend(trailer(&post.meta));

    let Budgeted { blocks, truncated } = fit(blocks, continue_link(&post.meta.post_id, base_url));

    Rendered {
        message: InputRichMessage::from_blocks(if blocks.is_empty() {
            vec![empty_notice(&post.meta.post_id, base_url)]
        } else {
            blocks
        }),
        truncated,
    }
}

/// Mengambil jatah foto sebanyak `count`; `false` berarti jatahnya tidak cukup.
///
/// Satu-satunya tempat jatah itu berkurang. Tanpa satu pintu seperti ini,
/// setiap cabang yang bisa menghasilkan foto harus ingat menghitungnya sendiri
/// — dan yang lupa tidak akan menghasilkan galat apa pun, cuma foto yang lolos
/// batas.
fn take_photos(photos: &mut usize, count: usize) -> bool {
    if *photos < count {
        return false;
    }

    *photos -= count;
    true
}

/// Pesan satu paragraf, tanpa apa pun yang lain.
///
/// Dipakai [`crate::bot`] untuk menjawab panggilan tamu yang gagal: panggilan
/// tamu **tidak punya `chat_id`**, jadi satu-satunya cara mengatakan "artikelnya
/// tidak bisa diambil" adalah lewat hasil inline itu sendiri. Karena itu
/// isinya harus selalu bisa dirender — satu paragraf teks polos, tanpa tautan,
/// tanpa media, dan tanpa blok kedua yang bisa membuat anggarannya lewat.
///
/// Teksnya datang dari dua tempat yang tidak dikendalikan di sini: kalimat
/// [`crate::bot`] sendiri, dan deskripsi galat dari Telegram atau `/api/v1`.
/// Yang kedua bisa panjang, tapi tidak bisa melewati [`budget::MAX_BYTES`] —
/// dan kalau suatu hari bisa, yang salah adalah tempat yang membiarkannya
/// sepanjang itu.
#[must_use]
pub fn one_paragraph(text: &str) -> Rendered {
    Rendered {
        message: InputRichMessage::from_blocks(vec![InputRichBlock::Paragraph {
            text: RichText::plain(text),
        }]),
        truncated: false,
    }
}

/// Satu blok untuk post yang tidak menghasilkan apa-apa.
///
/// Telegram menolak `{"blocks":[]}` dengan *"Rich message must be non-empty"* —
/// jadi jalur ini bukan kehati-hatian teoretis, ia satu-satunya yang memisahkan
/// sebuah post dari pesan yang gagal terkirim sama sekali.
///
/// Post seperti itu bukan hal yang dikarang: judul, subjudul, penulis, tanggal,
/// dan tag bisa kosong sekaligus pada post yang dipangkas atau dihapus Medium,
/// dan `medium-doc/src/parse.rs` memang pernah menghasilkan `P` berteks kosong
/// yang [`has_substance`] buang. Yang tersisa dari post semacam itu cuma idnya —
/// dan idnya cukup untuk mengantar pembaca ke halaman yang merendernya.
fn empty_notice(post_id: &str, base_url: &str) -> InputRichBlock {
    InputRichBlock::Paragraph {
        text: RichText::parts(vec![
            RichText::plain("Artikel ini tidak punya isi yang bisa ditampilkan. "),
            RichText::node(RichNode::Url {
                text: RichText::plain("Buka di web"),
                url: format!("{}/p/{post_id}", base_url.trim_end_matches('/')),
            }),
        ]),
    }
}

/// Gambar pratinjau, judul, subjudul, dan byline — lihat tabel di catatan modul.
///
/// Gambar pratinjaunya yang pertama mengambil jatah dari `photos`, dan itu
/// disengaja: halaman menampilkannya di atas judul, jadi kalau cuma satu foto
/// yang boleh dikirim, inilah yang paling menentukan hasilnya terlihat seperti
/// artikel. Lihat [`rich_message_with_photos`].
fn header(meta: &MetaDto, photos: &mut usize) -> Vec<InputRichBlock> {
    let mut blocks = Vec::new();

    if let Some(url) = non_empty(meta.preview_image_url.as_deref())
        && take_photos(photos, 1)
    {
        blocks.push(photo(url, None));
    }

    if !meta.title.trim().is_empty() {
        blocks.push(InputRichBlock::Heading {
            text: RichText::plain(&meta.title),
            // `size` di sini memang 1, dan itu judul artikel — bukan hasil
            // pergeseran `level` heading mana pun. Heading di dalam `blocks`
            // memakai `level` apa adanya; lihat [`block`].
            size: 1,
        });
    }

    // Miring, karena halaman menampilkannya sebagai `<h2>` yang lebih besar dan
    // abu-abu — dan `RichText` tidak punya "lebih kecil". Miring adalah satu-
    // satunya penanda yang tersedia yang membedakannya dari isi artikel.
    if let Some(subtitle) = non_empty(meta.subtitle.as_deref()) {
        blocks.push(InputRichBlock::Paragraph {
            text: RichText::node(RichNode::Italic {
                text: RichText::plain(subtitle),
            }),
        });
    }

    if let Some(byline) = byline(meta) {
        blocks.push(InputRichBlock::Footer { text: byline });
    }

    blocks
}

/// `{penulis} · ~N min read · {terbit} (Updated: {diperbarui})`.
///
/// Halaman memisahkan penulis ke kartunya sendiri dan menyisakan baris ini untuk
/// penerbitan, waktu baca, dan tanggal. Telegram tidak punya kartu penulis, jadi
/// namanya digabung ke baris yang sama — bagian yang hilang cuma avatarnya.
///
/// Kata-katanya sengaja **Inggris**, sama seperti `post.html`: ini teks halaman
/// yang dipindahkan, bukan kalimat bot. Yang benar-benar kalimat bot — penutup
/// saat artikel dipotong — berbahasa Indonesia, lihat [`continue_link`].
fn byline(meta: &MetaDto) -> Option<RichText> {
    /// Menambahkan satu bagian, dengan ` · ` di depannya kalau bukan yang
    /// pertama — pemisah yang sama dengan `post.html`.
    fn push(parts: &mut Vec<RichText>, part: RichText) {
        if !parts.is_empty() {
            parts.push(RichText::plain(" · "));
        }
        parts.push(part);
    }

    let mut parts: Vec<RichText> = Vec::new();

    if let Some(creator) = &meta.creator
        && !creator.name.trim().is_empty()
    {
        let name = RichText::plain(&creator.name);
        push(
            &mut parts,
            match non_empty(Some(&creator.username)) {
                Some(username) => RichText::node(RichNode::Url {
                    text: name,
                    url: format!("{PROFILE_BASE}{username}"),
                }),
                None => name,
            },
        );
    }

    if let Some(collection) = &meta.collection
        && !collection.name.trim().is_empty()
    {
        push(&mut parts, RichText::plain(&collection.name));
    }

    if meta.reading_time_minutes > 0 {
        push(
            &mut parts,
            RichText::plain(format!("~{} min read", meta.reading_time_minutes)),
        );
    }

    if let Some(date) = published(meta) {
        push(&mut parts, RichText::plain(date));
    }

    (!parts.is_empty()).then(|| RichText::parts(parts))
}

/// `September 6, 2026 (Updated: September 11, 2026)`, persis kalimat halaman.
///
/// Formatnya bukan milik bot ini: [`medium_doc::metadata::human_readable_date`]
/// yang menulisnya di halaman, dan fungsi itu dipanggil di sini alih-alih
/// disalin. Menyalinnya berarti dua implementasi yang bisa berbeda diam-diam,
/// dan "diam-diam berbeda dari halaman" justru hal yang bot ini ada untuk
/// menghindari.
fn published(meta: &MetaDto) -> Option<String> {
    let first = medium_doc::metadata::human_readable_date(meta.first_published_at_unix_ms?);

    Some(match meta.updated_at_unix_ms {
        Some(updated) => format!(
            "{first} (Updated: {})",
            medium_doc::metadata::human_readable_date(updated)
        ),
        None => first,
    })
}

/// Chip `#tag` di kaki halaman.
///
/// Halaman memakai `normalizedTagSlug`, bukan `displayTitle`, dan itu yang
/// dipakai di sini — `#web-development` cocok dengan tautan
/// `medium.com/tag/web-development` yang ada di halaman, sedangkan
/// `#Web Development` tidak menuju ke mana pun.
fn trailer(meta: &MetaDto) -> Option<InputRichBlock> {
    let tags: Vec<String> = meta
        .tags
        .iter()
        .map(|tag| format!("#{}", tag.slug.as_deref().unwrap_or(&tag.display_title)))
        .collect();

    (!tags.is_empty()).then(|| InputRichBlock::Footer {
        text: RichText::plain(tags.join(" ")),
    })
}

/// Penutup pesan yang dipotong.
///
/// Bahasanya Indonesia karena ini **bot** yang bicara, bukan halaman yang
/// dipindahkan. Tautannya menuju halaman artikel di deployment ini, tempat
/// sisanya bisa dibaca utuh.
fn continue_link(post_id: &str, base_url: &str) -> InputRichBlock {
    InputRichBlock::Paragraph {
        text: RichText::Parts(vec![
            RichText::plain("… "),
            RichText::node(RichNode::Url {
                text: RichText::plain("Baca selengkapnya"),
                url: format!("{}/p/{post_id}", base_url.trim_end_matches('/')),
            }),
        ]),
    }
}

/// Satu blok isi, atau `None` kalau Telegram tidak punya apa pun untuk
/// ditampilkan.
///
/// # Kenapa `None` ada
///
/// Blok yang tidak menghasilkan apa-apa **bukan** hal teoretis: `medium-render`
/// mencatat bahwa `unknowns.json` memuat `P` dengan teks kosong, dan `ImageRow`
/// tanpa gambar adalah jalan kedua. Blok kosong yang tetap dikirim bukan cuma
/// satu baris kosong — ia `paragraph` ber-`text` kosong, dan itu kandidat
/// penolakan validasi. [`has_substance`] yang memutuskan.
///
/// # `level` dipakai apa adanya
///
/// `Heading` di dalam isi artikel menjadi `size` yang **sama**: `H4` di Medium
/// jadi `size` 4. Ini bukan kelalaian. Artikel nyata memakainya secara terbalik
/// — kicker di kepala artikel adalah `H4`, sedangkan judul bagian `H3` — dan
/// memetakan `level` ke ukuran lain akan membalik hierarki yang justru sedang
/// direproduksi.
///
/// Jatah `photos` dikurangi di sini, bukan di [`rich_message_with_photos`]:
/// cuma cabang yang benar-benar menghasilkan gambar yang tahu berapa fotonya —
/// satu untuk `Image`, sebanyak isinya untuk `ImageRow`.
fn block(block: &BlockDto, photos: &mut usize) -> Option<InputRichBlock> {
    match block {
        BlockDto::Heading { level, content, .. } => Some(InputRichBlock::Heading {
            text: inline(content),
            // `id` dibuang: `RichText` tidak punya jangkar. Batasnya 1..=6
            // karena `size` 7 bukan heading.
            size: (*level).clamp(1, 6),
        }),

        // `content`, bukan `text`: `text` sudah diratakan `freedium-dto` dan
        // membuang tautan serta penekanan di dalam heading. Untuk daftar isi
        // itu tepat; untuk pesan yang harus sama dengan halaman, tidak.
        BlockDto::Paragraph { content, .. } => {
            has_substance(content).then(|| InputRichBlock::Paragraph {
                text: inline(content),
            })
        }

        BlockDto::List { ordered, items } => {
            let items: Vec<InputRichBlockListItem> = items
                .iter()
                .filter(|item| has_substance(item))
                .enumerate()
                .map(|(index, item)| {
                    let blocks = vec![InputRichBlock::Paragraph { text: inline(item) }];
                    match ordered {
                        // Ordinalnya dihitung dari 1 dan berurutan, sama seperti
                        // `medium-render/src/markdown.rs`: nomor di sumber tidak
                        // berarti apa-apa, dan yang membedakan list berurut dari
                        // list peluru adalah field `type` di butirnya.
                        true => InputRichBlockListItem::numbered(blocks, index as i32 + 1),
                        false => InputRichBlockListItem::bullet(blocks),
                    }
                })
                .collect();

            (!items.is_empty()).then_some(InputRichBlock::List { items })
        }

        BlockDto::Code { language, lines } => {
            let text = lines.join("\n");
            (!text.is_empty()).then(|| InputRichBlock::Pre {
                text: RichText::plain(text),
                // `freedium-dto` memetakan `None` ke `nohighlight` di halaman,
                // dan `nohighlight` bukan nama bahasa — mengirimnya sebagai
                // `language` akan jadi penolakan validasi.
                language: language
                    .clone()
                    .filter(|language| !language.trim().is_empty()),
            })
        }

        BlockDto::Blockquote { style, content } => {
            if !has_substance(content) {
                return None;
            }
            Some(match style {
                // `inset` adalah `<blockquote>` biasa di halaman.
                QuoteStyleDto::Inset => InputRichBlock::Blockquote {
                    blocks: vec![InputRichBlock::Paragraph {
                        text: inline(content),
                    }],
                    credit: None,
                },
                // `pull` adalah `<aside>`: lebih besar, abu-abu, menjorok. Itu
                // memang `pullquote`, bukan `blockquote`.
                QuoteStyleDto::Pull => InputRichBlock::Pullquote {
                    text: inline(content),
                    credit: None,
                },
            })
        }

        BlockDto::Image { url, alt, caption } => {
            take_photos(photos, 1).then(|| photo(url, caption_of(caption, alt)))
        }

        BlockDto::ImageRow { images } => {
            // Diperiksa lebih dulu, dan itu yang membuat barisnya utuh atau
            // hilang sama sekali — lihat [`rich_message_with_photos`]. Baris
            // kosong tetap tidak menghasilkan blok, karena jatah nol selalu
            // cukup dan `blocks` di bawahnya lalu kosong.
            if !take_photos(photos, images.len()) {
                return None;
            }

            let blocks: Vec<InputRichBlock> = images
                .iter()
                .map(|image| photo(&image.url, alt_caption(&image.alt)))
                .collect();

            (!blocks.is_empty()).then_some(InputRichBlock::Collage {
                blocks,
                caption: None,
            })
        }

        BlockDto::Embed {
            url,
            title,
            description,
            ..
        } => Some(InputRichBlock::Paragraph {
            text: embed_text(url, title, description),
        }),

        BlockDto::Iframe { src, .. } => Some(InputRichBlock::Paragraph {
            text: iframe_text(src),
        }),
    }
}

/// Satu gambar sebagai blok `photo`.
fn photo(url: &str, caption: Option<RichText>) -> InputRichBlock {
    InputRichBlock::Photo {
        photo: InputMediaPhoto::from_url(url),
        // Telegram mengunduh `media` sendiri, jadi URL `miro.medium.com`
        // diteruskan apa adanya — proksi `/@miro/` di `base.html` itu urusan
        // peramban, bukan sesuatu yang bisa dilihat Telegram.
        caption: caption.map(RichBlockCaption::new),
    }
}

/// Caption sebuah gambar: caption aslinya, atau `alt` kalau captionnya tidak ada.
///
/// **`alt` sebagai caption adalah divergensi.** Halaman tidak pernah
/// menampilkan `alt` kecuali gambarnya gagal dimuat. Di Telegram, gambar yang
/// gagal diunduh adalah hal yang biasa — `miro.medium.com` tidak menjanjikan
/// apa pun pada pengunduh otomatis — dan caption adalah satu-satunya tempat
/// teks itu masih berguna.
fn caption_of(caption: &Option<Vec<InlineDto>>, alt: &str) -> Option<RichText> {
    match caption {
        Some(content) if has_substance(content) => Some(inline(content)),
        _ => alt_caption(alt),
    }
}

fn alt_caption(alt: &str) -> Option<RichText> {
    (!alt.trim().is_empty()).then(|| RichText::plain(alt))
}

/// Kartu tautan sebagai satu paragraf: tautan tebal, lalu ringkasannya.
///
/// Telegram tidak punya blok kartu, jadi tidak ada bentuk yang benar di sini —
/// yang ada cuma pilihan tentang apa yang diselamatkan. Yang diselamatkan adalah
/// **URL dan ringkasannya**, karena itu isi kartunya. Yang dibuang:
///
/// - `site` — hostname yang sama sudah terlihat di URL-nya, dan `markdown.rs`
///   membuangnya dengan alasan yang sama.
/// - `thumbnail_url` — satu jatah media untuk hiasan, di artikel yang jatahnya
///   cuma 50. Sebuah gambar pratinjau yang gagal diunduh juga jauh lebih
///   mengganggu daripada tidak ada gambar sama sekali.
///
/// Judulnya tebal **di dalam** tautannya, bukan di sebelahnya: itu yang membuat
/// ia terbaca sebagai judul kartu, bukan sebagai kalimat yang kebetulan
/// mengandung tautan.
fn embed_text(url: &str, title: &str, description: &str) -> RichText {
    let label = match title.trim() {
        "" => url,
        title => title,
    };

    let mut parts = vec![RichText::node(RichNode::Url {
        text: RichText::node(RichNode::Bold {
            text: RichText::plain(label),
        }),
        url: url.to_string(),
    })];

    if !description.trim().is_empty() {
        // Satu paragraf dengan baris baru, bukan dua blok: keduanya bagian dari
        // satu kartu, dan memisahkannya jadi dua blok berarti pemotongan bisa
        // mengambil tautannya sambil menyisakan ringkasan yang menggantung.
        parts.push(RichText::plain("\n"));
        parts.push(RichText::plain(description));
    }

    RichText::parts(parts)
}

/// Iframe sebagai tautan.
///
/// Tidak ada blok iframe, dan tidak akan pernah ada: yang dirender halaman
/// adalah **situs lain**, lengkap dengan skripnya. Yang bisa dilakukan bot
/// adalah memberi pembaca jalan ke sana.
///
/// `src` dipakai sebagai labelnya, dan itu memang tidak enak dibaca ketika ia
/// menunjuk `{host_address}/render_iframe/{id}` — URL deployment ini sendiri,
/// bukan YouTube. Mencari nama yang lebih baik berarti menebak dari bentuk URL,
/// dan tebakan yang salah lebih buruk daripada URL yang jujur.
fn iframe_text(src: &str) -> RichText {
    RichText::node(RichNode::Url {
        text: RichText::plain(src),
        url: src.to_string(),
    })
}

/// Rangkaian inline jadi satu [`RichText`].
///
/// Pemetaannya satu lawan satu dan mekanis; satu-satunya yang tidak mekanis
/// adalah pembungkusan array, dan itu urusan [`RichText::parts`].
fn inline(content: &[InlineDto]) -> RichText {
    RichText::parts(content.iter().map(inline_one).collect())
}

fn inline_one(node: &InlineDto) -> RichText {
    match node {
        InlineDto::Text { text } => RichText::plain(text),

        InlineDto::Strong { content } => RichText::node(RichNode::Bold {
            text: inline(content),
        }),
        InlineDto::Emphasis { content } => RichText::node(RichNode::Italic {
            text: inline(content),
        }),
        InlineDto::Code { content } => RichText::node(RichNode::Code {
            text: inline(content),
        }),
        // `marked`, bukan `spoiler`: `spoiler` menyembunyikan teksnya sampai
        // disentuh, kebalikan dari stabilo Medium.
        InlineDto::Highlight { content } => RichText::node(RichNode::Marked {
            text: inline(content),
        }),

        // `rel`, `title`, dan `new_tab` dibuang: `RichNode::Url` tidak punya
        // tempat untuknya.
        InlineDto::Link { href, content, .. } => RichText::node(RichNode::Url {
            text: inline(content),
            url: href.clone(),
        }),

        // URL-nya dibentuk, bukan diambil: `https://medium.com/u/{user_id}`
        // adalah satu-satunya URL yang bisa dimaksud sebuah mention.
        InlineDto::UserMention { user_id, content } => RichText::node(RichNode::Url {
            text: inline(content),
            url: format!("{MENTION_BASE}{user_id}"),
        }),
    }
}

/// Apakah rangkaian inline ini akan menghasilkan sesuatu yang bisa dilihat atau
/// disentuh pembaca.
///
/// Bukan `content.is_empty()`. Sebuah `Text` berisi hanya spasi, atau `Strong`
/// yang isinya kosong, sama-sama tidak menampilkan apa pun — dan `paragraph`
/// ber-`text` kosong adalah kandidat penolakan validasi. Sebaliknya, `Link`
/// dengan label kosong **bukan** kosong: tujuannya nyata, dan membuangnya
/// berarti membuang isi.
fn has_substance(content: &[InlineDto]) -> bool {
    content.iter().any(|inline| match inline {
        InlineDto::Text { text } => !text.trim().is_empty(),

        InlineDto::Link { href, content, .. } => {
            !href.is_empty() || content.iter().any(has_inline_substance)
        }
        InlineDto::UserMention { user_id, content } => {
            !user_id.is_empty() || content.iter().any(has_inline_substance)
        }

        InlineDto::Strong { content }
        | InlineDto::Emphasis { content }
        | InlineDto::Code { content }
        | InlineDto::Highlight { content } => content.iter().any(has_inline_substance),
    })
}

/// [`has_substance`] satu tingkat lebih dalam, untuk isi sebuah pembungkus.
///
/// Sebuah pembungkus tidak membawa tujuan sendiri, jadi hanya teksnya yang
/// menghitung.
fn has_inline_substance(inline: &InlineDto) -> bool {
    match inline {
        InlineDto::Text { text } => !text.trim().is_empty(),
        InlineDto::Strong { content }
        | InlineDto::Emphasis { content }
        | InlineDto::Code { content }
        | InlineDto::Highlight { content } => content.iter().any(has_inline_substance),
        // Tautan bersarang tetap punya tujuan, jadi ia bernilai sama di sini.
        InlineDto::Link { href, content, .. } => {
            !href.is_empty() || content.iter().any(has_inline_substance)
        }
        InlineDto::UserMention { user_id, content } => {
            !user_id.is_empty() || content.iter().any(has_inline_substance)
        }
    }
}

/// `Some` hanya kalau teksnya ada isinya — `""` dan `"   "` sama-sama `None`.
///
/// `subtitle: Some("")` dan `None` memang berbeda di kontrak, tapi perbedaannya
/// hilang begitu sampai ke Telegram: keduanya satu paragraf kosong.
fn non_empty(text: Option<&str>) -> Option<&str> {
    text.filter(|text| !text.trim().is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use freedium_dto::{CreatorDto, ImageDto, TagDto};
    use serde_json::{Value, json};

    /// Jawaban sungguhan dari `/api/v1/posts/e6047997b667`, apa adanya.
    ///
    /// Disimpan di sini alih-alih dikarang karena seluruh gunanya adalah
    /// membuktikan bahwa kontrak yang **benar-benar** dikirim server bisa
    /// dikonsumsi. Bentuk yang diciptakan di dalam test tidak membuktikan itu;
    /// ia cuma membuktikan tebakan penulisnya konsisten dengan dirinya sendiri.
    const REAL_POST: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/fixtures/gpt-6-astra.json"
    ));

    const BASE_URL: &str = "https://medium.piyann.test";

    fn text(value: &str) -> InlineDto {
        InlineDto::Text {
            text: value.to_string(),
        }
    }

    fn paragraph(content: Vec<InlineDto>) -> BlockDto {
        BlockDto::Paragraph {
            content,
            drop_cap: false,
        }
    }

    fn image_block(url: &str) -> BlockDto {
        BlockDto::Image {
            url: url.to_string(),
            alt: String::new(),
            caption: None,
        }
    }

    fn meta(title: &str) -> MetaDto {
        MetaDto {
            schema_version: 1,
            post_id: "e6047997b667".to_string(),
            title: title.to_string(),
            subtitle: None,
            description: String::new(),
            preview_image_url: None,
            reading_time_minutes: 0,
            is_locked: false,
            medium_url: None,
            first_published_at_unix_ms: None,
            updated_at_unix_ms: None,
            tags: Vec::new(),
            creator: None,
            collection: None,
        }
    }

    fn post_with(meta: MetaDto) -> PostDto {
        PostDto {
            schema_version: 1,
            meta,
            blocks: Vec::new(),
        }
    }

    /// `PostDto` milik crate lain, jadi `with_blocks` harus fungsi bebas —
    /// `impl` inheren untuk tipe asing tidak boleh.
    fn with_blocks(mut post: PostDto, blocks: Vec<BlockDto>) -> PostDto {
        post.blocks = blocks;
        post
    }

    fn render(post: &PostDto) -> Rendered {
        rich_message(post, BASE_URL)
    }

    /// **Selalu lewat serialisasi.** Yang diuji adalah apa yang benar-benar
    /// terkirim, bukan bentuk tipe di memori — `RichText` yang tidak `untagged`
    /// akan lolos dari pemeriksaan tipe dan ditolak Telegram.
    fn json_of(block: &InputRichBlock) -> Value {
        serde_json::to_value(block).expect("blok terserialisasi")
    }

    /// Blok isi saja — kepala pesan selalu judul, jadi blok terakhir dari satu
    /// blok isi adalah blok isi itu.
    fn only(blocks: Vec<BlockDto>) -> Value {
        let rendered = render(&post("Judul", blocks));
        json_of(rendered.message.blocks.last().expect("ada blok isi"))
    }

    /// Apakah blok isi ini bertahan sampai ke pesan.
    fn survives(blocks: Vec<BlockDto>) -> bool {
        // Judul selalu ada, jadi "bertahan" berarti pesannya lebih panjang dari
        // satu blok itu.
        render(&post("Judul", blocks)).message.blocks.len() > 1
    }

    fn real() -> PostDto {
        serde_json::from_str(REAL_POST).expect("jawaban sungguhan API ada di fixtures")
    }

    fn null_paths(value: &Value, at: &str, out: &mut Vec<String>) {
        match value {
            Value::Null => out.push(at.to_string()),
            Value::Array(items) => {
                for (index, item) in items.iter().enumerate() {
                    null_paths(item, &format!("{at}[{index}]"), out);
                }
            }
            Value::Object(entries) => {
                for (key, entry) in entries {
                    null_paths(entry, &format!("{at}.{key}"), out);
                }
            }
            _ => {}
        }
    }

    fn post(title: &str, blocks: Vec<BlockDto>) -> PostDto {
        with_blocks(post_with(meta(title)), blocks)
    }

    // ---------------------------------------------------------------------
    // Inline
    // ---------------------------------------------------------------------

    #[test]
    fn a_lone_text_run_is_a_string_rather_than_a_one_element_array() {
        assert_eq!(
            only(vec![paragraph(vec![text("apa saja")])]),
            json!({ "type": "paragraph", "text": "apa saja" })
        );
    }

    /// Setiap varian `InlineDto` punya satu node yang berarti hal yang sama.
    /// Test ini memuat **semuanya** dalam satu tempat: `InlineDto` tidak
    /// `#[non_exhaustive]`, jadi menambah varian di `freedium-dto` akan
    /// menggagalkan kompilasi di sini — dan test inilah yang menjelaskan kenapa.
    #[test]
    fn every_inline_markup_maps_to_the_node_that_means_the_same_thing() {
        let wrapped = |node: InlineDto| only(vec![paragraph(vec![node])])["text"].clone();

        assert_eq!(
            wrapped(InlineDto::Strong {
                content: vec![text("b")]
            }),
            json!({ "type": "bold", "text": "b" })
        );
        assert_eq!(
            wrapped(InlineDto::Emphasis {
                content: vec![text("i")]
            }),
            json!({ "type": "italic", "text": "i" })
        );
        assert_eq!(
            wrapped(InlineDto::Code {
                content: vec![text("c")]
            }),
            json!({ "type": "code", "text": "c" })
        );

        // `marked`, bukan `spoiler`: `spoiler` menyembunyikan teksnya sampai
        // disentuh, kebalikan dari stabilo Medium.
        let highlight = wrapped(InlineDto::Highlight {
            content: vec![text("h")],
        });
        assert_eq!(highlight, json!({ "type": "marked", "text": "h" }));
        assert_ne!(highlight["type"], json!("spoiler"));
    }

    #[test]
    fn a_link_carries_its_destination_and_drops_what_rich_text_cannot_hold() {
        assert_eq!(
            only(vec![paragraph(vec![InlineDto::Link {
                href: "https://x.test/a".to_string(),
                rel: "noopener".to_string(),
                title: "sebuah judul".to_string(),
                new_tab: true,
                content: vec![text("label")],
            }])])["text"],
            json!({ "type": "url", "text": "label", "url": "https://x.test/a" })
        );
    }

    /// URL mention **dibentuk**, bukan diambil dari payload — dengan basis yang
    /// sama yang dipakai `medium-render`.
    #[test]
    fn a_user_mention_links_to_its_medium_profile() {
        assert_eq!(
            only(vec![paragraph(vec![InlineDto::UserMention {
                user_id: "fde1eb3eb589".to_string(),
                content: vec![text("@michal")],
            }])])["text"],
            json!({
                "type": "url",
                "text": "@michal",
                "url": "https://medium.com/u/fde1eb3eb589",
            })
        );
    }

    /// Penekanan di dalam tautan tetap satu pohon, tidak diratakan.
    #[test]
    fn nested_markup_keeps_its_shape() {
        assert_eq!(
            only(vec![paragraph(vec![InlineDto::Link {
                href: "https://x.test".to_string(),
                rel: String::new(),
                title: String::new(),
                new_tab: true,
                content: vec![InlineDto::Strong {
                    content: vec![text("tebal di dalam tautan")],
                }],
            }])])["text"],
            json!({
                "type": "url",
                "url": "https://x.test",
                "text": { "type": "bold", "text": "tebal di dalam tautan" },
            })
        );
    }

    // ---------------------------------------------------------------------
    // Blok isi
    // ---------------------------------------------------------------------

    fn heading(level: u8) -> Value {
        only(vec![BlockDto::Heading {
            level,
            id: "jangkar".to_string(),
            text: "datar".to_string(),
            content: vec![text("isi asli")],
        }])
    }

    /// **`size` adalah `level` itu sendiri**, dan test ini memakai pasangan yang
    /// benar-benar muncul di artikel sungguhan: kicker di kepala artikel adalah
    /// `H4`, sementara judul bagian `H3`. Menggeser `level` ke ukuran lain —
    /// misalnya `level - 1` supaya `H2` jadi terbesar — akan membalik hierarki
    /// yang justru sedang direproduksi.
    #[test]
    fn a_heading_keeps_its_level_as_its_size() {
        assert_eq!(heading(4)["size"], json!(4));
        assert_eq!(heading(3)["size"], json!(3));

        // `content`, bukan `text`: `text` sudah diratakan dan membuang markup.
        assert_eq!(heading(3)["text"], json!("isi asli"));
        assert!(
            !heading(3).to_string().contains("jangkar"),
            "`id` tidak punya bentuk di RichText"
        );
    }

    /// Heading dengan `level` di luar 1..=6 tetap dikirim sebagai heading:
    /// `size` 7 bukan heading, dan menolak seluruh pesan karena itu terlalu mahal.
    #[test]
    fn an_out_of_range_heading_level_is_clamped_rather_than_sent() {
        assert_eq!(heading(0)["size"], json!(1));
        assert_eq!(heading(7)["size"], json!(6));
    }

    /// Paragraf kosong adalah bentuk yang benar-benar dikirim Medium —
    /// `medium-render` mencatat `unknowns.json` memuat `P` berteks kosong — dan
    /// `paragraph` ber-`text` kosong adalah kandidat penolakan validasi.
    #[test]
    fn a_paragraph_with_nothing_to_show_is_dropped_rather_than_sent_empty() {
        assert!(!survives(vec![paragraph(vec![])]), "paragraf tanpa inline");
        assert!(
            !survives(vec![paragraph(vec![text("")])]),
            "satu teks kosong"
        );
        assert!(
            !survives(vec![paragraph(vec![text("   \n  ")])]),
            "hanya spasi"
        );
        assert!(
            !survives(vec![paragraph(vec![InlineDto::Strong { content: vec![] }])]),
            "pembungkus yang isinya kosong"
        );
    }

    /// **Kebalikannya juga benar**: tautan berlabel kosong bukan paragraf
    /// kosong. Tujuannya nyata, dan membuangnya berarti membuang isi.
    #[test]
    fn a_paragraph_holding_only_a_bare_link_survives() {
        assert!(survives(vec![paragraph(vec![InlineDto::Link {
            href: "https://x.test/a".to_string(),
            rel: String::new(),
            title: String::new(),
            new_tab: true,
            content: vec![],
        }])]));
    }

    /// Inilah satu-satunya yang membedakan list berurut dari list peluru di rich
    /// message — blok listnya sendiri tidak punya flag `ordered`.
    #[test]
    fn an_ordered_list_numbers_its_items_and_an_unordered_one_does_not() {
        let list = |ordered: bool| {
            only(vec![BlockDto::List {
                ordered,
                items: vec![vec![text("satu")], vec![text("dua")]],
            }])
        };

        let numbered = list(true);
        assert_eq!(numbered["type"], json!("list"));
        assert_eq!(numbered["items"][0]["type"], json!("1"));
        assert_eq!(numbered["items"][1]["type"], json!("1"));
        assert_eq!(numbered["items"][1]["value"], json!(2));

        let bulleted = list(false);
        assert!(
            !bulleted["items"][0]
                .as_object()
                .unwrap()
                .contains_key("type"),
            "butir peluru tidak punya gaya label sama sekali: {bulleted}"
        );
    }

    #[test]
    fn a_list_whose_items_all_rendered_empty_is_dropped() {
        assert!(!survives(vec![BlockDto::List {
            ordered: true,
            items: vec![vec![], vec![text("  ")]],
        }]));
    }

    #[test]
    fn code_keeps_its_language_and_joins_its_lines() {
        let pre = only(vec![BlockDto::Code {
            language: Some("rust".to_string()),
            lines: vec!["fn main() {".to_string(), "}".to_string()],
        }]);

        assert_eq!(pre["type"], json!("pre"));
        assert_eq!(pre["text"], json!("fn main() {\n}"));
        assert_eq!(pre["language"], json!("rust"));

        // `freedium-dto` memetakan `None` ke kelas CSS `nohighlight` di halaman;
        // itu bukan nama bahasa dan akan ditolak sebagai `language`.
        let plain = only(vec![BlockDto::Code {
            language: Some(String::new()),
            lines: vec!["x".to_string()],
        }]);
        assert!(plain.get("language").is_none(), "{plain}");

        assert!(!survives(vec![BlockDto::Code {
            language: None,
            lines: vec![],
        }]));
    }

    /// Dua gaya kutipan Medium adalah dua elemen berbeda di halaman, jadi harus
    /// dua blok berbeda di sini juga.
    #[test]
    fn the_two_quote_styles_become_the_two_different_blocks() {
        let quote = |style| {
            only(vec![BlockDto::Blockquote {
                style,
                content: vec![text("kutipan")],
            }])
        };

        assert_eq!(
            quote(QuoteStyleDto::Inset),
            json!({
                "type": "blockquote",
                "blocks": [{ "type": "paragraph", "text": "kutipan" }],
            })
        );
        assert_eq!(
            quote(QuoteStyleDto::Pull),
            json!({ "type": "pullquote", "text": "kutipan" })
        );
    }

    /// Caption menang atas `alt`, `alt` menang atas tidak ada apa-apa.
    #[test]
    fn an_image_caption_falls_back_to_its_alt_text() {
        let image = |caption: Option<Vec<InlineDto>>, alt: &str| {
            only(vec![BlockDto::Image {
                url: "https://miro.medium.com/v2/resize:fit:700/1*a.png".to_string(),
                alt: alt.to_string(),
                caption,
            }])
        };

        assert_eq!(
            image(Some(vec![text("caption asli")]), "teks alternatif")["caption"]["text"],
            json!("caption asli")
        );
        assert_eq!(
            image(None, "teks alternatif")["caption"]["text"],
            json!("teks alternatif")
        );
        assert!(image(None, "   ").get("caption").is_none());

        // URL-nya diteruskan apa adanya: Telegram yang mengunduh, dan proksi
        // `/@miro/` itu urusan peramban.
        assert_eq!(
            image(None, "a")["photo"]["media"],
            json!("https://miro.medium.com/v2/resize:fit:700/1*a.png")
        );
        assert_eq!(image(None, "a")["photo"]["type"], json!("photo"));
    }

    #[test]
    fn an_image_row_becomes_a_collage_and_an_empty_one_is_dropped() {
        let row = |images: Vec<ImageDto>| only(vec![BlockDto::ImageRow { images }]);

        let collage = row(vec![
            ImageDto {
                url: "https://x/1*1.png".to_string(),
                alt: "satu".to_string(),
            },
            ImageDto {
                url: "https://x/1*2.png".to_string(),
                alt: String::new(),
            },
        ]);

        assert_eq!(collage["type"], json!("collage"));
        assert_eq!(collage["blocks"].as_array().unwrap().len(), 2);
        assert!(collage["blocks"][1].get("caption").is_none());

        assert!(
            !survives(vec![BlockDto::ImageRow { images: vec![] }]),
            "kolase tanpa gambar bukan blok"
        );
    }

    /// Jatah foto membuang gambarnya, bukan memotong artikelnya.
    ///
    /// Itu bedanya dengan [`budget::fit`], dan alasan batas ini ada sama sekali:
    /// `fit` berhenti di blok pertama yang lewat anggaran, jadi menurunkan batas
    /// media lewatnya akan membuang seluruh sisa teks. Jalur tamu butuh
    /// kebalikannya — teksnya utuh, fotonya yang boleh hilang.
    #[test]
    fn a_photo_limit_drops_the_photos_and_keeps_the_rest_of_the_article() {
        let article = post(
            "Judul",
            vec![
                image_block("https://x/1*a.png"),
                paragraph(vec![text("sebelum")]),
                image_block("https://x/1*b.png"),
                paragraph(vec![text("sesudah")]),
            ],
        );

        let capped = |max_photos| rich_message_with_photos(&article, BASE_URL, max_photos);
        let photos = |rendered: &Rendered| budget::Cost::of_blocks(&rendered.message.blocks).media;
        let paragraphs = |rendered: &Rendered| {
            rendered
                .message
                .blocks
                .iter()
                .filter(|block| matches!(block, InputRichBlock::Paragraph { .. }))
                .count()
        };

        assert_eq!(photos(&capped(usize::MAX)), 2, "tanpa batas, semuanya ikut");
        assert_eq!(photos(&capped(1)), 1);
        assert_eq!(photos(&capped(0)), 0);

        for max_photos in [usize::MAX, 1, 0] {
            let rendered = capped(max_photos);

            assert_eq!(
                paragraphs(&rendered),
                2,
                "teksnya harus utuh pada jatah {max_photos}"
            );
            assert!(
                !rendered.truncated,
                "jatah foto bukan pemotongan, dan jangan dilaporkan sebagai pemotongan"
            );
        }
    }

    /// Batasnya juga harus berlaku pada jawaban server yang sungguhan.
    ///
    /// Test di atas memakai post karangan, dan post karangan membuktikan
    /// penalarannya konsisten dengan dirinya sendiri — bukan bahwa kontrak yang
    /// benar-benar dikirim `/api/v1` bisa dibatasi. Gambar di `REAL_POST` ada
    /// sebelas, jadi batasnya benar-benar menggigit di sini.
    #[test]
    fn a_photo_limit_holds_on_a_real_api_answer() {
        let article = real();
        let photos = |max_photos| {
            let rendered = rich_message_with_photos(&article, BASE_URL, max_photos);
            budget::Cost::of_blocks(&rendered.message.blocks).media
        };

        assert!(photos(usize::MAX) > 1, "fixture-nya memang bergambar");
        assert_eq!(photos(1), 1, "gambar pratinjau yang tersisa");
        assert_eq!(photos(0), 0);
    }

    /// **Divergensi yang disengaja.** Telegram tidak punya blok kartu, jadi yang
    /// diselamatkan adalah URL dan ringkasannya.
    #[test]
    fn an_embed_becomes_a_bold_link_and_its_description() {
        let embed = only(vec![BlockDto::Embed {
            url: "https://www.example.com/post".to_string(),
            title: "An Article".to_string(),
            description: "A description".to_string(),
            site: "sitename.invalid".to_string(),
            thumbnail_url: Some("https://x/1*t.png".to_string()),
        }]);

        assert_eq!(embed["type"], json!("paragraph"));
        assert_eq!(
            embed["text"],
            json!([
                {
                    "type": "url",
                    "url": "https://www.example.com/post",
                    "text": { "type": "bold", "text": "An Article" },
                },
                "\n",
                "A description",
            ])
        );

        let rendered = embed.to_string();
        assert!(
            !rendered.contains("sitename.invalid"),
            "hostname yang sama sudah terlihat di URL-nya"
        );
        assert!(
            !rendered.contains("1*t.png"),
            "thumbnail menghabiskan jatah media untuk hiasan"
        );
    }

    /// Tanpa judul, URL-nya sendiri yang jadi label — kalau tidak, tautannya
    /// tidak bisa dikenali.
    #[test]
    fn an_embed_without_a_title_uses_its_url_as_the_label() {
        let embed = only(vec![BlockDto::Embed {
            url: "https://www.example.com/post".to_string(),
            title: String::new(),
            description: "  ".to_string(),
            site: String::new(),
            thumbnail_url: None,
        }]);

        assert_eq!(
            embed["text"],
            json!({
                "type": "url",
                "url": "https://www.example.com/post",
                "text": {
                    "type": "bold",
                    "text": "https://www.example.com/post",
                },
            }),
            "ringkasan kosong tidak menyisakan baris kosong"
        );
    }

    /// **Divergensi yang disengaja.** Yang dirender halaman adalah situs lain
    /// lengkap dengan skripnya; yang bisa diberikan bot adalah jalan ke sana.
    #[test]
    fn an_iframe_becomes_a_link_to_its_source() {
        assert_eq!(
            only(vec![BlockDto::Iframe {
                src: "https://www.youtube.com/embed/x".to_string(),
                width: Some(640),
                height: Some(360),
            }]),
            json!({
                "type": "paragraph",
                "text": {
                    "type": "url",
                    "text": "https://www.youtube.com/embed/x",
                    "url": "https://www.youtube.com/embed/x",
                },
            })
        );
    }

    // ---------------------------------------------------------------------
    // Kepala dan kaki pesan
    // ---------------------------------------------------------------------

    /// Urutannya menyalin `post.html`, dan gambar pratinjau memang di atas `<h1>`
    /// di sana.
    #[test]
    fn the_message_opens_with_the_preview_image_then_the_title() {
        let mut meta = meta("GPT-6 Astra just ended software.");
        meta.preview_image_url =
            Some("https://miro.medium.com/v2/resize:fit:700/1*p.png".to_string());
        meta.subtitle = Some("And coding has NOTHING to do with that.".to_string());

        let blocks = render(&post_with(meta)).message.blocks;
        let head = json_of(&blocks[0]);
        let title = json_of(&blocks[1]);

        assert_eq!(head["type"], "photo");
        assert_eq!(title["type"], "heading");
        assert_eq!(title["size"], json!(1), "judul artikel");
        assert_eq!(title["text"], json!("GPT-6 Astra just ended software."));
        assert_eq!(
            blocks[2],
            InputRichBlock::Paragraph {
                text: RichText::node(RichNode::Italic {
                    text: RichText::plain("And coding has NOTHING to do with that."),
                }),
            },
            "subjudul halaman lebih besar dan abu-abu; miring satu-satunya penanda yang ada"
        );
    }

    /// Byline-nya adalah kalimat halaman itu sendiri, dengan penulisnya digabung
    /// ke dalamnya karena Telegram tidak punya kartu penulis.
    #[test]
    fn the_byline_repeats_the_pages_own_words_and_links_the_author() {
        let mut meta = meta("Judul");
        meta.reading_time_minutes = 9;
        meta.first_published_at_unix_ms = Some(1_788_724_643_048);
        meta.updated_at_unix_ms = Some(1_789_147_021_316);
        meta.creator = Some(CreatorDto {
            id: "fde1eb3eb589".to_string(),
            name: "Michal Malewicz".to_string(),
            username: "michalmalewicz".to_string(),
            bio: String::new(),
            image_url: None,
        });

        let blocks = render(&post_with(meta)).message.blocks;
        let byline = json_of(blocks.last().expect("kaki pesan"));

        assert_eq!(byline["type"], "footer");
        assert_eq!(
            byline["text"],
            json!([
                {
                    "type": "url",
                    "text": "Michal Malewicz",
                    "url": "https://medium.com/@michalmalewicz",
                },
                " · ",
                "~9 min read",
                " · ",
                "September 6, 2026 (Updated: September 11, 2026)",
            ]),
            "tanggalnya harus sama dengan yang dicetak halaman"
        );
    }

    /// Penulis tanpa `username` tetap muncul, hanya tanpa tautan — dan meta yang
    /// tidak punya apa-apa tidak menghasilkan byline sama sekali.
    #[test]
    fn a_byline_without_a_username_is_plain_text() {
        let mut meta = meta("Judul");
        meta.creator = Some(CreatorDto {
            id: "u1".to_string(),
            name: "Tanpa Nama Pengguna".to_string(),
            username: String::new(),
            bio: String::new(),
            image_url: None,
        });

        let blocks = render(&post_with(meta)).message.blocks;
        assert_eq!(
            json_of(blocks.last().expect("kaki pesan"))["text"],
            json!("Tanpa Nama Pengguna")
        );

        assert_eq!(
            render(&post("Judul", vec![])).message.blocks.len(),
            1,
            "meta kosong: cuma judul, tanpa byline"
        );
    }

    /// Chip `#tag` di kaki halaman memakai slug, bukan judul tampilannya:
    /// `#web-development` menuju ke suatu tempat, `#Web Development` tidak.
    #[test]
    fn the_tags_are_a_footer_of_slugs() {
        let mut meta = meta("Judul");
        meta.tags = vec![
            TagDto {
                display_title: "Web Development".to_string(),
                slug: Some("web-development".to_string()),
            },
            TagDto {
                display_title: "Design".to_string(),
                slug: None,
            },
        ];

        let blocks = render(&post_with(meta)).message.blocks;
        let tags = json_of(blocks.last().expect("kaki pesan"));

        assert_eq!(tags["type"], "footer");
        assert_eq!(tags["text"], json!("#web-development #Design"));
    }

    // ---------------------------------------------------------------------
    // Pemotongan
    // ---------------------------------------------------------------------

    /// Artikel yang tidak muat diakhiri tautan ke halaman ini — dan tidak ada
    /// blok yang muncul setelah blok yang tidak muat itu.
    #[test]
    fn an_article_too_long_to_fit_ends_with_a_link_to_its_own_page() {
        let long = "x".repeat(budget::MAX_BYTES / 2);
        let rendered = render(&post(
            "Panjang",
            vec![
                paragraph(vec![text(&long)]),
                paragraph(vec![text(&long)]),
                paragraph(vec![text("blok terakhir")]),
            ],
        ));

        assert!(rendered.truncated);
        assert_eq!(
            rendered.message.blocks.last(),
            Some(&InputRichBlock::Paragraph {
                text: RichText::Parts(vec![
                    RichText::plain("… "),
                    RichText::node(RichNode::Url {
                        text: RichText::plain("Baca selengkapnya"),
                        url: format!("{BASE_URL}/p/e6047997b667"),
                    }),
                ]),
            })
        );
    }

    #[test]
    fn a_short_article_gains_no_closing_link() {
        let rendered = render(&post("Pendek", vec![paragraph(vec![text("apa saja")])]));

        assert!(!rendered.truncated);
        assert!(
            !serde_json::to_string(&rendered.message)
                .expect("pesan terserialisasi")
                .contains("Baca selengkapnya")
        );
    }

    // ---------------------------------------------------------------------
    // Artikel sungguhan
    // ---------------------------------------------------------------------

    /// **Jawaban sungguhan dari API, dirender utuh.**
    ///
    /// Yang diperiksa bukan keluarannya, melainkan sifat yang harus berlaku untuk
    /// artikel apa pun — sama seperti test korpus di `medium-render/src/markdown.rs`,
    /// dan dengan alasan yang sama: tidak ada naskah acuan untuk pesan Telegram,
    /// jadi berkas emas hanya akan jadi pendapat modul ini yang ditulis dua kali.
    #[test]
    fn the_real_api_response_renders_every_block_and_fits() {
        let post = real();
        let rendered = render(&post);

        assert_eq!(post.schema_version, freedium_dto::SCHEMA_VERSION);
        assert_eq!(post.blocks.len(), 107, "korpusnya berubah; lihat fixture");

        assert!(
            !rendered.truncated,
            "artikel rujukan ini muat; kalau tidak, batasnya yang salah"
        );

        // Kepala: gambar pratinjau, judul, subjudul, byline. Kaki: tag. Tidak ada
        // blok isi yang boleh hilang diam-diam.
        assert_eq!(
            rendered.message.blocks.len(),
            post.blocks.len() + 5,
            "ada blok isi yang dibuang, dan itu harus disengaja"
        );

        let total = budget::Cost::of_blocks(&rendered.message.blocks);
        assert!(
            total.fits_within(budget::Cost::ceiling()),
            "pesan yang dirender melewati batas: {total:?}"
        );
    }

    /// **Tidak ada `null` di mana pun.**
    ///
    /// Setiap `Option` di [`model`] di-`skip_serializing_if`, jadi `null` di
    /// keluaran berarti ada satu yang lupa — dan satu `"html": null` sudah cukup
    /// membuat Telegram membaca pesan ini sebagai dua mode sekaligus.
    #[test]
    fn nothing_in_the_rendered_message_is_null() {
        let value = serde_json::to_value(&render(&real()).message).expect("pesan terserialisasi");

        let mut failures = Vec::new();
        null_paths(&value, "rich_message", &mut failures);
        assert!(failures.is_empty(), "null di: {}", failures.join(", "));
    }

    /// Artikel rujukan memuat `H4` di kepala dan `H3` untuk judul bagian —
    /// hierarki terbalik yang memang begitu adanya di Medium, dan yang harus
    /// bertahan apa adanya supaya pesannya sama dengan halaman.
    #[test]
    fn the_real_article_keeps_its_inverted_heading_hierarchy() {
        let blocks = render(&real()).message.blocks;

        let sizes: Vec<u8> = blocks
            .iter()
            .filter_map(|block| match block {
                InputRichBlock::Heading { size, .. } => Some(*size),
                _ => None,
            })
            .collect();

        assert_eq!(sizes.first(), Some(&1), "yang pertama adalah judul artikel");
        assert_eq!(sizes.get(1), Some(&4), "kicker artikel ini memang `H4`");
        assert!(sizes.contains(&3), "judul bagiannya `H3`");
    }

    /// Gambar artikel rujukan tidak punya caption, jadi inilah yang membuktikan
    /// `alt` benar-benar sampai ke pembaca alih-alih hilang.
    #[test]
    fn the_real_articles_images_keep_their_alt_text_as_a_caption() {
        let blocks = render(&real()).message.blocks;

        let captions: Vec<String> = blocks
            .iter()
            .filter_map(|block| match block {
                InputRichBlock::Photo {
                    caption: Some(caption),
                    ..
                } => Some(caption.text.flatten()),
                _ => None,
            })
            .collect();

        assert_eq!(
            captions.len(),
            11,
            "sebelas gambar isi; pratinjau tanpa caption"
        );
        assert!(
            captions.iter().all(|caption| !caption.trim().is_empty()),
            "caption kosong tidak boleh lolos"
        );
    }

    // ---------------------------------------------------------------------
    // Post yang tidak menghasilkan apa-apa
    // ---------------------------------------------------------------------

    /// Telegram menolak `{"blocks":[]}` dengan *"Rich message must be non-empty"*,
    /// jadi ini bukan kehati-hatian teoretis: tanpanya, satu post yang seluruh
    /// metadatanya kosong berakhir sebagai pengiriman yang gagal.
    #[test]
    fn a_post_with_nothing_in_it_still_produces_one_block() {
        let rendered = render(&post_with(meta("")));

        assert_eq!(rendered.message.blocks.len(), 1);
        assert!(!rendered.truncated, "tidak ada yang dipotong di sini");
    }

    /// Yang tersisa dari post semacam itu cuma idnya, dan idnya harus bisa
    /// ditekan — `skip_entity_detection` mematikan deteksi tautan otomatis, jadi
    /// URL yang ditulis sebagai teks tidak akan bisa dibuka.
    #[test]
    fn the_block_for_an_empty_post_links_to_our_own_page() {
        let rendered = render(&post_with(meta("")));

        assert_eq!(
            json_of(&rendered.message.blocks[0]),
            json!({
                "type": "paragraph",
                "text": [
                    "Artikel ini tidak punya isi yang bisa ditampilkan. ",
                    {
                        "type": "url",
                        "url": "https://medium.piyann.test/p/e6047997b667",
                        "text": "Buka di web",
                    },
                ],
            })
        );
    }

    /// Judul saja sudah cukup untuk membuat pesan yang berarti, jadi jalur ini
    /// tidak boleh ikut menyalanya — pesan kosong yang sebenarnya berisi judul
    /// akan jadi bug yang tidak terlihat.
    #[test]
    fn a_post_with_only_a_title_does_not_get_the_empty_notice() {
        let rendered = render(&post("Judul", Vec::new()));

        assert_eq!(rendered.message.blocks.len(), 1);
        assert!(
            !serde_json::to_string(&rendered.message)
                .expect("pesan terserialisasi")
                .contains("tidak punya isi"),
            "notis kosongnya menyalakan jalur yang salah"
        );
    }
}
