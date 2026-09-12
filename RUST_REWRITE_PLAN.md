# Rencana Rewrite Freedium ke Rust

Status: draft untuk review
Basis kode: commit `75d1433`

> **Catatan path (2026-09-12).** Seluruh kode Python sudah dipindah ke `legacy/`.
> Semua rujukan path di dokumen ini (`medium-parser/...`, `web/server/...`,
> `database-lib/...`, `rl_string_helper/...`, `tests/...`) mengacu ke tata letak
> **sebelum** pemindahan itu — tambahkan prefiks `legacy/` untuk menemukan
> filenya sekarang. Nomor baris masih valid: `git mv` tidak mengubah isi file.
> Di dalam container, path tetap `/app/web`, `/app/medium-parser`, dst.

---

## 0. Framing jujur dulu

Motivasi rewrite ini **bukan throughput**. Freedium adalah proxy I/O-bound yang
dibentengi dua lapis cache; latensi didominasi oleh Medium GraphQL API dan
cache hit Redis, bukan oleh CPU Python. Yang realistis didapat dari Rust:

1. **Menghapus satu kelas bug secara struktural.** `rl_string_helper` melakukan
   splicing string sambil memelihara `string_pos_matrix` (peta posisi), plus
   "bang char" untuk mensimulasikan offset UTF-16. Hampir semua isu render
   historis (emoji geser, karakter aneh di title, drop cap ganda) berasal dari
   sini. Desain IR di §2.1 membuat kelas bug ini tidak mungkin terjadi.
2. **Satu binary statis**, tanpa gunicorn/uvicorn/preload_app/worker_abort,
   memori turun drastis (sekarang `mem_limit: 4g` per container).
3. **Membuka 2 item roadmap yang sekarang mentok**: render ke Markdown
   (`core.py:814` masih `NotImplementedError`) dan JSON API untuk frontend
   Svelte di `new-web/`.
4. **Satu bahasa dari edge sampai parser.** Caddy (Go) + HAProxy (C) + app
   (Python) jadi Pingora (Rust) + app (Rust), dengan HAProxy dihapus total
   (§2.8).

Risiko terbesar bukan soal bahasa, tapi **parity TLS impersonation** (§3.1).
Itu harus di-spike sebelum satu baris pun kode produksi ditulis.

---

## 1. Inventaris yang di-port

| Komponen | LOC Python | Target Rust | Catatan |
|---|---:|---|---|
| `medium-parser/medium_parser/core.py` | 817 | `medium-doc` + `medium-render` | dipecah, bukan diterjemahkan 1:1 |
| `medium-parser/medium_parser/utils.py` | 450 | `medium-doc::resolve` | resolver URL + daftar domain |
| `medium-parser/medium_parser/api.py` | 101 | `medium-client` | **titik risiko utama** |
| `medium-parser/medium_parser/markups.py` | 59 | `medium-doc::inline` | hilang, jadi bagian IR |
| `rl_string_helper/` (+ `.pyx`) | ~520 | **dihapus**, tidak di-port | diganti IR, lihat §2.1 |
| `database-lib/database_lib/main.py` | 455 | `freedium-cache` | kontrak DB tidak berubah |
| `web/server/` | ~800 | `freedium-web` | axum |
| `web/server/templates/*.html` | 6 file | **dipakai apa adanya** | minijinja, lihat §2.2 |

Total ±3.200 LOC Python → perkiraan 5.000–6.500 LOC Rust.

### Peta dependensi

| Python | Rust | Keyakinan |
|---|---|---|
| `curl_cffi` (impersonate chrome110) | `rquest` | **rendah — spike dulu** |
| `fastapi` + `uvicorn` + `gunicorn` | `axum` + `tokio` + `tower-http` | tinggi |
| `jinja2` | `minijinja` | tinggi |
| `psycopg2` | `sqlx` (postgres, runtime-tokio) | tinggi |
| `redis-py` + `hiredis` | `fred` | tinggi |
| `orjson` | `serde_json` (opsi: `sonic-rs`) | tinggi |
| `loguru` + `@trace` | `tracing` + `#[instrument]` | tinggi |
| `sentry-sdk[fastapi]` | `sentry` + `sentry-tower` + `sentry-tracing` | tinggi |
| `html5lib` parse/serialize | `html5ever` (kandidat dihapus, §2.3) | sedang |
| `beautifulsoup4` (patch iframe) | `lol_html` | tinggi |
| `tld.get_fld` | `psl` | tinggi |
| `difflib.SequenceMatcher.ratio` | `difflib` crate — **wajib divalidasi**, §3.2 | sedang |
| `async_lru` / `lru_cache` | `moka` | tinggi |
| `aiohttp_socks` | fitur `socks` di reqwest/rquest | tinggi |
| `pickledb` (`ban_post_list.db`) | `serde_json` → pindah ke tabel Postgres | tinggi |
| `xkcdpass` (X-Request-ID) | wordlist di-embed, atau ganti ULID | tinggi |
| `caddy` (edge) | `pingora` (§2.8) | sedang |
| `haproxy` (balance SOCKS5) | **dihapus**, pool masuk `medium-client` (§2.8) | tinggi |
| — (belum ada OpenAPI) | `utoipa` + scalar/swagger-ui | tinggi |
| — (belum ada rate limit) | token bucket di edge Pingora (§2.7) | tinggi |

---

## 2. Keputusan arsitektur

### 2.1 Document IR — inti dari rewrite ini

Pipeline sekarang render HTML **dua kali lewat** dan menyambung string:

1. `markups.py` render template dengan `DebugUndefined`, sehingga `{{text}}`
   sengaja **dibiarkan literal** di output → jadi template untuk lewat kedua.
2. `rl_string_helper` menyisipkan template itu ke posisi byte tertentu, sambil
   menggeser `string_pos_matrix` setiap kali ada insert, dan menambah/menghapus
   "bang char" `R` untuk mengkompensasi offset UTF-16 dari Medium.

Ganti seluruhnya dengan IR:

```rust
pub struct Document {
    pub meta: PostMeta,
    pub blocks: Vec<Block>,
}

pub enum Block {
    Heading { level: u8, id: String, inline: Vec<Inline> },
    Paragraph { inline: Vec<Inline>, drop_cap: bool },
    List { ordered: bool, items: Vec<Vec<Inline>> },
    Code { lang: Option<String>, text: String },
    BlockQuote { style: QuoteStyle, inline: Vec<Inline> },
    Image { id: String, alt: String, caption: Option<Vec<Inline>>, layout: Layout },
    ImageRow(Vec<Block>),
    Embed { url: String, title: String, description: String, site: String, thumb: Option<String> },
    Iframe { src: String, width: Option<u32>, height: Option<u32> },
}

pub enum Inline {
    Text(String),
    Strong(Vec<Inline>),
    Emphasis(Vec<Inline>),
    Code(Vec<Inline>),
    Link { href: String, title: String, rel: String, new_tab: bool, children: Vec<Inline> },
    UserMention { user_id: String, children: Vec<Inline> },
    Highlight(Vec<Inline>),
}
```

Mengapa ini menyelesaikan masalah:

- **Offset UTF-16 ditangani sekali, di batas parsing.** Markup Medium memakai
  offset UTF-16 code unit. Bangun `Vec<usize>` pemetaan utf16-index → byte-index
  dari `str::char_indices()` + `char::len_utf16()`, sekali per paragraf, lalu
  konversi semua range markup ke byte range. Tidak ada bang char, tidak ada
  matriks yang digeser-geser, tidak ada mutasi.
- **Overlap diselesaikan jadi tree, bukan splice.** `split_overlapping_ranges`
  sudah memotong range jadi non-overlapping — tapi hasilnya lalu di-splice ke
  string. Di IR, range non-overlapping langsung jadi anak-anak `Inline` secara
  berurutan: emit sekali jalan, tanpa menghitung ulang posisi.
- **Escaping HTML sekali saja, di renderer.** Sekarang `quote_html` jalan di
  tengah pipeline dengan mode `full`/`minimal`, dan hasil escape-nya harus
  ikut menggeser matriks posisi. Di IR, `Inline::Text` menyimpan teks mentah;
  escaping terjadi hanya saat emit. Blok `Code` cukup pakai mode minimal.
- **Lewat kedua template hilang.** `Inline::Link { href, children }` di-emit
  langsung; tidak perlu `DebugUndefined` untuk mengawetkan `{{text}}`.
- **Markdown & JSON jadi gratis.** `Document` → `render_html()`,
  `render_markdown()`, `Serialize` untuk API `new-web/`.

Ini juga berarti **`rl_string_helper` dan ekstensi Cython-nya tidak di-port,
tapi dihapus** — beserta `string_pos_matrix`, `UTF16Handler`, dan
`StringAssignmentMixin`. Test suite-nya (255 baris) tetap berharga: dipakai
sebagai sumber kasus uji untuk renderer IR, bukan sebagai spesifikasi API.

### 2.2 Template Jinja2 dipakai ulang tanpa diubah

`minijinja` kompatibel dengan subset Jinja2 yang dipakai di sini (`{% if %}`,
`{% for %}`, `{{ x or y }}`, `{% raw %}`, filter dasar). 6 template di
`web/server/templates/` — termasuk `base.html` (23 KB, Tailwind + JS tema +
highlight.js) — dipindahkan apa adanya dan di-embed ke binary lewat
`minijinja-embed`. Frontend tidak berubah sama sekali selama rewrite, jadi
perbandingan output di §4 valid.

Konsekuensi: `new-web/` (SvelteKit) **tetap di luar scope**. Rewrite frontend
adalah proyek terpisah yang jadi jauh lebih mudah setelah IR ada.

### 2.3 Jangan bawa round-trip html5lib

`handlers/main.py:60` dan `handlers/post.py:99` mem-parse lalu men-serialize
ulang seluruh HTML dengan html5lib, semata untuk merapikan markup rusak yang
dihasilkan splicing string. Dengan generasi dari IR, output sudah well-formed.

Rencana: **pertahankan** round-trip (via `html5ever`) selama fase differential
testing supaya output bisa dibandingkan apple-to-apple, lalu **hapus** setelah
byte-parity tercapai. Ini satu-satunya bagian yang CPU-bound di request path,
jadi menghapusnya adalah perbaikan latensi p99 yang nyata.

### 2.4 Kontrak penyimpanan

**Postgres — tidak ada migrasi.** Tabel `cache (key TEXT PRIMARY KEY, value TEXT)`
menyimpan respons GraphQL sebagai JSON teks. Netral bahasa, Rust baca langsung.
Dua hal yang harus direplikasi:
- `CacheData.workaround_decode_json` (`database-lib/database_lib/main.py:37`):
  sebagian nilai ternyata bytea hex-encoded dengan prefiks `\x`. Reader Rust
  harus menangani kedua bentuk.
- `push()` memakai `INSERT ... ON CONFLICT DO UPDATE`.

**Redis — namespace baru, tanpa migrasi.** Sekarang `post.py:66` menyimpan
`pickle.dumps(HtmlResult)` di key `{post_id}` mentah. Rust tidak bisa membaca
pickle Python. Karena TTL hanya 5 jam (`CACHE_LIFE_TIME`), cukup pakai prefiks
baru `v2:post:{id}` dan biarkan key lama kedaluwarsa sendiri. Format baru:
MessagePack (`rmp-serde`) atas struct `RenderedPost`. Idem untuk
`aio_redis_cache` → `v2:homepage`.

Ini artinya Python dan Rust bisa **jalan bersamaan** berbagi Postgres yang sama
tanpa saling mengganggu di Redis — prasyarat untuk §5.

**`ban_post_list.db`** → tabel `banned_posts` di Postgres. (Meski namanya
pickledb, formatnya JSON; verifikasi isi file dulu, kalau memang JSON bisa
dibaca serde_json untuk proses migrasi sekali jalan.)

### 2.5 Satu proses, bukan N worker

Ganti gunicorn preload + N UvicornWorker dengan satu binary axum di tokio
multi-thread runtime. Yang ikut hilang: `post_worker_init` (hack unregister
atexit), `worker_abort` (dump stack saat timeout), `worker_exit`, dan
`number_of_workers()`. Timeout per-request yang sekarang di
`middlewares/logger.py:52` (`asyncio.wait_for(call_next, TIMEOUT)`) jadi
`tower_http::timeout::TimeoutLayer`.

### 2.6 Struktur workspace

```
crates/
  freedium-web/       [bin] axum, router, middleware, template, config
  freedium-dto/       [lib] tipe kontrak publik API — stabil, versioned (§2.7)
  medium-client/      [lib] fetch TLS-impersonating + GraphQL + pool proxy
  medium-doc/         [lib] GraphQL JSON -> Document IR, resolver URL
  medium-render/      [lib] Document -> HTML | Markdown | DTO
  freedium-cache/     [lib] backend Postgres + Redis
xtask/
  difftest/                harness pembanding Python vs Rust (§4)

edge/                      workspace TERPISAH — lihat §3.4 soal boring-sys
  freedium-edge/      [bin] pingora: static, denylist, cache, rate limit
```

`medium-client` dibuat satu-satunya crate yang menyentuh jaringan keluar, dan
di belakang trait `PostSource`. Itu seam yang dipakai untuk fallback di §3.1.

`freedium-dto` dipisah dari `medium-doc` dengan sengaja: IR internal bebas
di-refactor, sementara DTO adalah kontrak yang di-generate jadi client
pihak ketiga dan tidak boleh berubah semaunya (§2.7).

### 2.7 API publik yang bisa dikonsumsi

#### Kondisi sekarang

**Tidak ada API.** Yang ada hanya `POST /report-problem` dan
`POST /delete-from-cache` (admin). Lebih penting: **`/api*` diblokir keras di
edge** — `caddy/Caddyfile:154` dan `caddy/generate_caddy_file.py:27` memasukkan
`api*` ke `ACCESS_DENIED_PATHS`, jadi respons 403 sebelum sampai ke app.
Membuka API **wajib** mencabut entri itu; kalau tidak, endpoint baru di axum
tidak akan pernah terpanggil di dev maupun prod.

#### Bentuk endpoint

```
GET  /api/v1/posts/{id}                 Document IR penuh (JSON)
GET  /api/v1/posts/{id}/html            fragmen HTML artikel (tanpa base.html)
GET  /api/v1/posts/{id}/markdown        text/markdown
GET  /api/v1/posts/{id}/meta            metadata saja — murah, untuk link preview
GET  /api/v1/resolve?url=...            URL apa pun -> {post_id, canonical_url}
GET  /api/v1/feed?limit=&cursor=        pengganti homepage random
GET  /api/v1/health                     liveness + status dependensi
GET  /api/v1/openapi.json               spec, di-generate utoipa
GET  /api/v1/docs                       scalar UI
```

`/resolve` adalah endpoint yang nilainya paling sering diremehkan: logika di
`medium_parser/utils.py` (450 baris) sudah menangani shortlink `link.medium.com`,
redirect tracking Google/Facebook/12ft, path `/p/`, custom domain, dan ekstraksi
post_id heksadesimal. Itu berguna berdiri sendiri dan tidak ada padanannya.

#### Kontrak & versioning

- **DTO bukan IR.** `Document` internal di-map ke tipe di `freedium-dto`.
  Alasannya: sekarang `post.py:94` dan `core.py:707` meneruskan dict mentah
  Medium (`creator`, `collection`, `tags`) langsung ke template. Kalau itu
  dibocorkan ke API publik, bentuk GraphQL internal Medium jadi kontrak publik
  Freedium — dan setiap perubahan di sisi Medium langsung jadi breaking change
  bagi konsumen. Definisikan DTO sendiri.
- `#[serde(tag = "type")]` untuk `Block`/`Inline`, plus field
  `"schema_version": 1` di root. Kebijakan: **hanya penambahan**; field baru
  opsional, penghapusan/rename butuh `/api/v2`.
- Error pakai RFC 9457 `application/problem+json`, bukan
  `{"message": ...}` seperti sekarang di `handlers/misc.py`. Mapping dari
  exception yang ada: `InvalidURL`/`NotValidMediumURL` → 400,
  `InvalidMediumPostURL`/`MediumPostQueryError` → 404,
  `PageLoadingError` → 502, timeout → 504.

#### Cross-cutting yang wajib ada sejak hari pertama

| Aspek | Implementasi | Alasan |
|---|---|---|
| CORS | `tower-http::cors`, allowlist konfigurabel | konsumen browser & extension |
| `ETag` + `If-None-Match` | hash konten dari IR | isi artikel praktis immutable; 304 itu gratis |
| `Cache-Control` | `public, max-age=...` sesuai `CACHE_LIFE_TIME` | biar bisa di-cache Pingora (§2.8) & CDN |
| Rate limit | token bucket per-IP di edge | lihat peringatan di bawah |
| `X-RateLimit-*`, `Retry-After` | header di 429 | supaya konsumen bisa backoff benar |
| API key (opsional) | tabel `api_keys`, simpan hash | tier lebih tinggi tanpa buka pintu anonim |
| Pagination | cursor, bukan offset | `/feed` |
| `X-Request-ID` | dipertahankan dari middleware sekarang | korelasi log/Sentry |

#### Dua peringatan operasional

1. **Jangan publikasikan API tanpa rate limit.** Sekarang tidak ada rate
   limiting sama sekali di seluruh stack. Setiap cache miss memakai pool WARP
   (`wgcf1` → `haproxy-pb`), dan komit `1d6b9d3` sudah menurunkan pool ke satu
   instance. API publik tanpa batas = scraper akan menghabiskan satu-satunya
   exit IP, lalu Medium memblokirnya, lalu **seluruh situs mati** — bukan cuma
   API-nya.
2. **Jika `MEDIUM_AUTH_COOKIES` diisi, jangan layani API anonim dengannya.**
   Env itu memegang cookie akun Medium berlangganan (`uid`/`sid`). Respons
   GraphQL membawa `MeteringInfoData { maxUnlockCount unlocksRemaining }` —
   artinya unlock itu ada kuotanya dan terikat ke akun. API publik yang
   memakai jalur berkuota ini akan menghabiskan kuota akun tersebut dan
   berisiko kena tindakan dari Medium. Aturan: request ber-auth-cookie hanya
   untuk tier terautentikasi, atau matikan endpoint API saat cookie dikonfigurasi.

### 2.8 Edge tier: Caddy → Pingora

#### Yang sebenarnya dikerjakan Caddy sekarang

Setelah dibaca, tugasnya kecil:

1. `encode gzip`, `header -Server`
2. Serve 18 file statis dari `caddy/static/` (~500 KB, didominasi
   `tailwindcssv3-freedium-hotfix.js` 398 KB)
3. Denylist 22 path → 403 (noise scanner: `/wp-*`, `/.git/*`, `/.env`, dll)
4. `reverse_proxy freedium_web:7080` + set `Host`, `X-Real-IP`,
   `X-Forwarded-For`, `X-Forwarded-Proto`
5. `lb_try_duration 30s` / `lb_try_interval 1s`
6. `tls internal` untuk hostname `.local` — **hanya dev**

#### Kabar baik: ACME bukan masalah di sini

Keberatan utama terhadap Pingora biasanya "tidak ada ACME/auto-HTTPS bawaan
seperti Caddy". Di repo ini itu tidak berlaku:

- `caddy/Caddyfile:194` mem-403-kan `/.well-known/*` — challenge ACME HTTP-01
  tidak mungkin lewat. Jadi Caddy **tidak** menerbitkan sertifikat di prod.
- README §"Production run" menegaskan prod berjalan di belakang reverse proxy
  eksternal, dan Caddy hanya listen plain HTTP `:6752`.
- `tls internal` cuma dipakai untuk `freedium.local`/`plausible.freedium.local`.

Artinya TLS diterminasi di lapisan luar. Pingora tinggal melayani HTTP biasa.
Untuk dev, ganti `tls internal` dengan sertifikat self-signed yang di-generate
`rcgen` saat start (perilaku sama: browser tetap warning, README sudah bilang
abaikan).

#### Pertanyaan yang jujur: perlu Pingora atau cukup axum?

Poin 1–5 di atas semuanya bisa masuk ke app axum dalam ~50 baris
(`CompressionLayer`, `ServeDir` + `include_dir!`, match path untuk denylist,
`SetResponseHeaderLayer`). Kalau tujuannya cuma "hapus Caddy", itu jalur
termurah dan menghapus satu container tanpa dependensi baru.

Pingora layak dipakai kalau yang diinginkan adalah **edge tier terpisah yang
bisa di-upgrade independen**, karena ia memberi tiga hal yang axum tidak:

- **`pingora-cache`** — melayani HTML yang sudah di-cache **tanpa membangunkan
  proses app sama sekali**. Untuk Freedium ini bukan fitur tambahan, ini memang
  beban kerja intinya: proxy konten ber-cache. Sekarang setiap request, termasuk
  cache hit, melewati seluruh middleware Python + round-trip Redis.
- **Graceful upgrade** — fitur andalan Pingora (`SO_REUSEPORT` + handoff fd,
  proses lama drain sampai selesai). Relevan langsung di sini: `TIMEOUT=38s`,
  `stop_grace_period: 2m`, healthcheck `--max-time 80`. Deploy sekarang memutus
  request yang sedang jalan.
- **Rate limiting di edge** — prasyarat §2.7, dan lebih baik dilakukan sebelum
  request memakan worker app.

Ditambah: connection pooling & h2 ke upstream, dan denylist dievaluasi sebelum
app tersentuh.

**Rekomendasi:** ya pakai Pingora, tapi **sebagai fase terpisah setelah app-nya
sudah Rust** (Fase 7). Menjalankan dua migrasi sekaligus membuat difftest §4
tidak bisa menyalahkan siapa pun saat ada diff.

#### Yang harus dibangun di `freedium-edge`

Pingora adalah *library*, bukan binary siap pakai — tidak ada Caddyfile DSL,
tidak ada file server, tidak ada ACME. Semuanya kode:

```rust
impl ProxyHttp for FreediumEdge {
    // request_filter:  denylist -> 403, static -> serve dari include_dir!, rate limit -> 429
    // upstream_peer:   pilih freedium_web:7080
    // request_filter_upstream: set Host / X-Real-IP / X-Forwarded-For / X-Forwarded-Proto
    // response_filter: hapus header Server, set Cache-Control
    // cache hooks:     cache_key dari post_id, hormati Cache-Control dari app
}
```

Yang **hilang** dan harus diterima atau diganti: config DSL (jadi kode +
file TOML), auto-HTTPS (tidak dipakai, lihat di atas), dan Caddyfile
per-hostname routing (jadi match biasa). `CaddyfileMaintance` (halaman
maintenance) jadi satu handler statis — lebih baik, karena sekarang menukarnya
butuh ganti file config + reload.

#### HAProxy dihapus, tidak diganti

`proxy-balancer/haproxy/haproxy.cfg` adalah `mode tcp`, `balance roundrobin`,
`bind *:1080` → `wgcf1:1080`. Ini **L4 SOCKS5**, bukan HTTP — Pingora adalah
framework proxy HTTP dan bukan alat yang tepat untuk ini.

Jawaban yang benar: `medium-client` sudah memilih proxy sendiri
(`api.py:33`, `random.choice(self.proxy_list)`). Pindahkan pool ke dalam
crate itu — daftar endpoint SOCKS5 langsung ke `wgcf1..N`, health check aktif
(`https://www.cloudflare.com/cdn-cgi/trace` → cek `warp=on`, persis seperti
healthcheck compose sekarang), eject backend yang gagal, round-robin di antara
yang sehat. Hasilnya lebih baik dari HAProxy karena klien tahu **request mana
yang gagal karena proxy** dan bisa retry di exit lain — informasi yang tidak
dimiliki balancer L4. Satu container hilang.

---

## 3. Risiko & spike yang wajib dulu

### 3.1 SPIKE-1: TLS impersonation (blocking, 2–3 hari)

Ini risiko yang bisa membatalkan rencana. `api.py:75` mengandalkan
`impersonate="chrome110"` dari `curl_cffi` — tanpa fingerprint TLS/HTTP2 yang
menyerupai browser, `medium.com/_/graphql` memblokir request. Tidak ada
padanan first-party di Rust.

Uji berurutan, berhenti di yang pertama berhasil:

1. **`rquest`** (BoringSSL, profil impersonation chrome/safari). Jalankan 500
   request `FullPostQuery` lewat pool WARP yang sama, bandingkan success rate
   dengan `curl_cffi` sebagai baseline. Gate: **≥99% parity**.
2. **Bind libcurl-impersonate lewat FFI.** `curl-impersonate` sudah dipakai
   secara tidak langsung (curl_cffi adalah binding-nya). Link `.so`-nya dari
   Rust. Lebih berat di build, tapi fingerprint-nya persis identik.
3. **Fallback arsitektural: sidecar fetcher.** Simpan proses Python kecil
   (hanya `curl_cffi` + FastAPI, ±80 LOC) yang cuma mengekspos
   `POST /fetch/{post_id}` → JSON GraphQL mentah. Rust memanggilnya lewat
   trait `PostSource`. Jelek secara estetika, tapi **semua nilai rewrite di
   §0 tetap didapat**, karena parser dan renderer — sumber semua bug — tetap
   pindah ke Rust. Risikonya terisolasi di 80 baris.

Keputusan: jika (1) dan (2) gagal, **tetap lanjut** dengan (3). Jangan
membatalkan rewrite karena satu masalah fingerprint TLS.

### 3.2 SPIKE-2: parity `difflib.ratio()` (1 hari)

`core.py:261` dan `core.py:276` memakai
`difflib.SequenceMatcher(None, a, b).ratio() > 80` untuk memutuskan apakah
paragraf pertama adalah duplikat title/subtitle yang harus dibuang. Kalau
ratio Rust berbeda sedikit saja, artikel akan kehilangan heading atau
menampilkan title dua kali.

Dua jebakan: algoritmanya Ratcliff/Obershelp (bukan Levenshtein — `strsim`
**tidak** cocok), dan `autojunk` Python aktif secara default untuk sekuens
≥200 elemen sehingga bisa mengubah rasio pada subtitle panjang.

Metode: ekstrak 50k pasangan (paragraph.text, title) dari korpus §4, hitung
rasio di kedua implementasi, dan wajibkan **keputusan boolean di threshold 80
identik 100%** — bukan rasio yang identik.

### 3.3 Risiko lain

| Risiko | Mitigasi |
|---|---|
| Korpus tidak mewakili edge case langka | Ambil sampel acak + seluruh `blacklist_url` dan URL bertanda "still have problems" di `tests/smokie_tests.py` |
| Tailwind class drift antara renderer lama/baru | Class di-hardcode di IR renderer; ditangkap difftest sebagai diff HTML |
| Struktur GraphQL Medium berubah di tengah migrasi | Parser IR harus tolerant: field tidak dikenal → skip + `tracing::warn`, jangan gagal seluruh artikel |
| Long tail kasus render tak terduga | Itulah fase 4; alokasi waktunya memang paling besar |
| API publik menghabiskan satu-satunya exit IP WARP | Rate limit **sebelum** API dibuka, bukan sesudah (§2.7) |
| Konsumen API terikat ke bentuk GraphQL Medium | DTO terpisah dari IR, kebijakan hanya-penambahan (§2.7) |
| Pingora tanpa ACME | Tidak relevan — TLS diterminasi di luar (§2.8) |

### 3.4 SPIKE-3: konflik `boring-sys` (0.5 hari, sebelum Fase 7)

`rquest` (kandidat §3.1) dan `pingora` keduanya bisa memakai BoringSSL lewat
`boring-sys`. Cargo menyatukan versi dependensi per lockfile, jadi kalau
keduanya menuntut versi `boring-sys` yang berbeda, satu workspace tidak akan
resolve — dan build BoringSSL itu lambat serta butuh toolchain C++.

Mitigasi yang sudah masuk ke §2.6: `edge/` adalah **workspace terpisah** dengan
`Cargo.lock` sendiri. Cek di awal Fase 7 apakah versinya kebetulan cocok; kalau
ya, boleh disatukan, tapi jangan jadikan asumsi.

Catatan build: image Pingora lebih berat dari `caddy:2-alpine` (39 MB). Siapkan
Dockerfile multi-stage dengan cache `cargo-chef`, dan pertimbangkan glibc
(bukan musl) karena BoringSSL di Alpine sering menyusahkan.

---

## 4. Differential testing — gerbang penerimaan

Ini mekanisme kualitas utama, dan sudah tersedia gratis: tabel Postgres `cache`
berisi puluhan/ratusan ribu respons GraphQL Medium asli. Itu korpus regresi.

```
xtask/difftest:
  1. dump N=20.000 baris (key, value) dari tabel cache
     — acak, + semua post_id yang disebut di smokie_tests.py
  2. render tiap value dengan parser Python (subprocess, offline, tanpa jaringan)
  3. render tiap value dengan renderer Rust
  4. normalisasi kedua output (collapse whitespace, urutkan atribut)
  5. laporkan: identik / beda-kosmetik / beda-semantik
```

Gate rilis:
- **0 beda semantik** (teks hilang, link salah, urutan blok berubah, escaping bocor)
- Beda kosmetik ditinjau manual satu per satu, lalu di-snapshot sebagai baseline
- Seluruh URL di `smokie_tests.py` HTTP 200 dengan title non-kosong

Keuntungan penting: langkah 2 dan 3 **tidak menyentuh jaringan**, jadi harness
bisa jalan di CI berulang kali tanpa memicu rate limit Medium dan tanpa
bergantung pada hasil SPIKE-1.

---

## 5. Fase pengerjaan

Strategi: **strangler fig**, bukan big bang. Caddy sudah ada di depan
(`caddy/Caddyfile`), dan Postgres sudah jadi kontrak netral bahasa (§2.4), jadi
kedua implementasi bisa hidup berdampingan dan dibandingkan dengan trafik asli.

### Fase 0 — Spike (3–5 hari)
- SPIKE-1 (§3.1) dan SPIKE-2 (§3.2)
- Setup workspace cargo + CI
- **Keluaran: keputusan `PostSource` mana yang dipakai.**

### Fase 1 — IR + renderer + difftest (2–3 minggu) ← bagian terberat
- `medium-doc`: GraphQL JSON → `Document`, termasuk peta offset UTF-16
- `medium-render`: `Document` → HTML, class Tailwind persis sama
- `medium-doc::resolve`: port `utils.py` (daftar domain, link pendek, redirect
  Google/Facebook/12ft, ekstraksi post_id)
- `xtask/difftest` jalan dan gate §4 hijau
- Tanpa jaringan, tanpa server. Murni fungsi murni + test.

### Fase 2 — Cache & client (1 minggu)
- `freedium-cache`: reader/writer Postgres (termasuk kasus hex `\x`),
  Redis namespace `v2:`
- `medium-client`: sesuai keputusan Fase 0, retry+backoff
- **Pool proxy in-process** (§2.8): daftar SOCKS5 `wgcf1..N`, health check
  aktif via `cdn-cgi/trace`, eject/retry backend gagal → HAProxy bisa dimatikan
- Migrasi `ban_post_list.db` → tabel `banned_posts`

### Fase 3 — Server HTML (1 minggu)
- axum: route catch-all `/{path}`, `/@miro/*`, `/render_iframe/*`,
  `POST /delete-from-cache`, `POST /report-problem`
- Template minijinja di-embed, halaman error, homepage
- `tracing` + Sentry + notifier Telegram
- `CompressionLayer` + `ServeDir` untuk `caddy/static/` (biar app bisa jalan
  mandiri tanpa edge saat dev)
- Dockerfile multi-stage (`cargo-chef` → distroless)
- **Belum ada API.** Paritas dulu, fitur baru belakangan — supaya difftest §4
  membandingkan dua hal yang sama.

### Fase 4 — Shadow traffic (2–3 minggu) ← long tail ada di sini
- Deploy `freedium_web_rust` sebagai service baru di compose, share Postgres
- Caddy mem-mirror trafik GET ke instance Rust, **respons dibuang**;
  user tetap dilayani Python
- Bandingkan output secara kontinu, perbaiki diff sampai kering
- Bandingkan p50/p99, memori, error rate
- Gate lanjut: 7 hari berturut tanpa diff semantik pada trafik nyata

### Fase 5 — Cutover (1 minggu)
- Caddy alihkan 5% → 25% → 100% trafik ke Rust
- Rollback = ubah satu baris Caddyfile (Python tetap jalan sepanjang fase ini)
- Setelah 2 minggu stabil: hapus service Python, hapus round-trip html5lib (§2.3)

### Fase 6 — API publik (1,5–2 minggu)
Baru dimulai **setelah** Rust melayani 100% trafik HTML dan stabil 2 minggu.
- `freedium-dto` + mapping `Document` → DTO, `schema_version: 1`
- Endpoint §2.7, `problem+json`, ETag/`If-None-Match`, `Cache-Control`, CORS
- `render_markdown()` → tutup item roadmap
- Rate limit sementara di app (`tower_governor`) sampai Fase 7 memindahkannya
  ke edge
- `utoipa` → `/api/v1/openapi.json` + `/api/v1/docs`
- `/api/v1/feed` sekaligus mengganti `ORDER BY RANDOM()` di homepage (§7 item 2)
- **Cabut `api*` dari `ACCESS_DENIED_PATHS`** di `caddy/generate_caddy_file.py`
  dan regenerate Caddyfile — tanpa ini endpoint tidak akan pernah tercapai
- Gate rilis: rate limit terbukti jalan (uji beban), dan keputusan soal
  `MEDIUM_AUTH_COOKIES` (§2.7 peringatan 2) sudah diambil
- Setelah stabil: sambungkan `new-web/` ke `/api/v1/`

### Fase 7 — Edge: Caddy → Pingora (1,5–2 minggu)
Independen dari Fase 6, tapi rate limit sebaiknya sudah ada lebih dulu.
- SPIKE-3 (§3.4) dulu
- `edge/freedium-edge`: denylist, static via `include_dir!`, header upstream,
  `header -Server`, gzip
- `pingora-cache` untuk respons HTML & API yang ber-`Cache-Control`
- Rate limit per-IP dipindah dari app ke edge
- Sertifikat dev self-signed via `rcgen` (pengganti `tls internal`)
- Halaman maintenance jadi handler, pengganti `CaddyfileMaintance`
- Cutover: jalankan berdampingan, Caddy tetap ada, tukar port di compose;
  rollback = tukar balik
- Setelah stabil: hapus `caddy/`, `proxy-balancer/`, `docker-compose.proxy.yml`,
  dan `bin/x86_64/caddy` (39 MB binary yang di-commit)

**Total: ±11–14 minggu** untuk semuanya. Jalur kritis sampai "Rust melayani
100% trafik" tetap ±8–10 minggu (Fase 0–5); Fase 6 dan 7 adalah tambahan di
atas itu dan bisa dikerjakan paralel oleh orang berbeda kalau ada.

Fase 1 dan 4 tetap 60% dari effort inti. Fase 0 bisa membatalkan atau mengubah
bentuk Fase 2, jadi jangan kerjakan paralel.

---

## 6. Yang TIDAK di-rewrite

Jangan sentuh, di luar scope:

- `docker-compose/`, `wgcf/` — infra tetap (kecuali menghapus service
  `haproxy-proxy-balancer` di Fase 2, `caddy_freedium*` di Fase 7, dan
  `pgadmin4_freedium` — yang terakhir **tidak terikat fase apa pun**: rewrite
  tidak pernah menyentuhnya, tapi ia dihapus 2026-09-12 karena portnya
  (`5433:80`) bentrok dengan `klon-pg` yang justru ditunjuk `DATABASE_URL`
  host-run, dan karena ia UI admin Postgres berkredensial `root`/`root` tanpa
  key `profiles:` sehingga ikut terangkat di profil default — yaitu produksi)
- `plausible/` — analytics. Catatan: Caddy sekarang juga mem-proxy
  `freedium_plausible:8000` di `:6753`; Pingora harus meneruskan itu, atau
  Plausible di-expose langsung
- `new-web/` — rewrite frontend adalah proyek terpisah. Fase 6 menyediakan
  API-nya, penyambungannya menyusul
- `freedium-library/` — refactor Python DI yang sedang jalan. **Ini keputusan
  yang perlu konfirmasi**: rewrite Rust membuat `freedium-library` redundan.
  Rekomendasi: hentikan pekerjaan di sana, tapi pakai `utils/utils/utf_handler.py`
  dan `mutable_string.py` beserta test-nya sebagai referensi semantik UTF-16
  saat mengerjakan Fase 1.
- `sitemap-generator/` — script terpisah, biarkan Python

Masuk scope (perubahan dari draft sebelumnya):

- `caddy/` — diganti `edge/freedium-edge` di Fase 7, dihapus setelah stabil
- `proxy-balancer/` — dihapus di Fase 2, fungsinya masuk `medium-client`
- `caddy/generate_caddy_file.py` — dicabut entri `api*` di Fase 6, lalu ikut
  terhapus di Fase 7 (denylist jadi konstanta Rust)

---

## 7. Diperbaiki saat port, jangan direplikasi

Ditemukan saat pembacaan kode. Semuanya harus **tidak** ikut pindah:

1. **`medium-parser/medium_parser/api.py:85`** — setiap respons GraphQL ditulis
   ke `/app/web/sidufh.json`. Debug code yang tertinggal; menulis ke disk pada
   setiap cache miss. Hapus.
2. **`database-lib/database_lib/main.py:336`** — `ORDER BY RANDOM() LIMIT n`
   untuk homepage. Full scan di tabel yang bisa berisi juta-an baris, dipanggil
   tiap 10 menit saat cache homepage expired. Ganti dengan `TABLESAMPLE SYSTEM`
   atau tabel `featured_posts` yang di-refresh berkala.
3. **`medium-parser/medium_parser/core.py:172`** — `while ... else` dengan
   `break`: alurnya benar tapi sangat sulit dibaca, dan `reason` bisa
   ter-set padahal query berhasil. Di Rust jadi loop retry eksplisit yang
   mengembalikan `Result`.
4. **`medium-parser/medium_parser/utils.py:167`** `correct_url()` — menghitung
   `unquerified_url` dan `unplaginated_url` lalu **mengembalikan `url` asli**.
   Kedua transformasi itu dibuang. Entah bug entah sengaja; klarifikasi dulu
   sebelum port, karena memengaruhi cache key.
5. **`resolve_medium_url`** mengembalikan `False` (bool) di jalur gagal dan
   `str` di jalur sukses. Di Rust jadi `Option<PostId>`.
6. **`web/server/utils/notify.py:20`** — `status: MessageStatus = "ERROR"`,
   default berupa string tapi dibandingkan dengan `MessageStatus.GOOD.value`.
   Bekerja karena kebetulan. Jadi enum sungguhan di Rust.
7. **`handlers/main.py:37`** — pengecekan `ADMIN_SECRET_KEY` membandingkan
   string secara langsung. Pakai perbandingan constant-time (`subtle`).
8. **`utils/error.py` / `post.py:102`** — `send_message` sukses dipanggil di
   setiap request berhasil lalu dibuang di dalam fungsi. Hapus dari hot path.

---

## 8. Yang perlu diputuskan sebelum mulai

1. **`freedium-library` dihentikan atau diteruskan?** (§6) — memengaruhi apakah
   ada dua rewrite paralel.
2. **Target `curl_cffi`**: kalau SPIKE-1 opsi 1 & 2 gagal, setuju dengan sidecar
   Python (§3.1 opsi 3), atau rewrite ditunda?
3. **`correct_url()`** (§7 item 4): transformasi yang dibuang itu bug atau
   sengaja?
4. **Byte-parity atau semantic-parity** sebagai gate rilis? Rekomendasi:
   semantic-parity, dengan diff kosmetik ditinjau manual. Byte-parity memaksa
   mereplikasi quirk html5lib dan akan memperpanjang Fase 1 secara signifikan.
5. **API: anonim atau butuh key?** (§2.7) Rekomendasi: anonim dengan rate limit
   ketat per-IP untuk endpoint baca, API key untuk limit lebih tinggi. Perlu
   angka konkret — usul awal 30 req/menit anonim, dan **hanya cache hit** di
   atas itu (cache miss dibatasi lebih keras karena memakai exit WARP).
6. **`MEDIUM_AUTH_COOKIES` + API publik** (§2.7 peringatan 2) — ini keputusan
   kebijakan, bukan teknis, dan perlu diambil sebelum Fase 6 dimulai. Apakah
   instance produksi `freedium.cfd` mengisi env ini sekarang?
7. **Pingora atau cukup absorb ke axum?** (§2.8) Rekomendasi: Pingora, tapi
   alasannya harus `pingora-cache` + graceful upgrade + rate limit di edge.
   Kalau tiga itu tidak dianggap penting, memindahkan tugas Caddy ke axum
   lebih murah dan tetap menghapus satu container.
8. **Plausible di belakang edge baru atau di-expose langsung?** (§6) — Caddy
   sekarang mem-proxy `:6753` ke `freedium_plausible:8000`.
