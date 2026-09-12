# legacy/ — implementasi Python pra-rewrite

Seluruh kode Python Freedium yang lama ada di sini. Isinya **bukan** kode mati:
sampai rewrite Rust selesai, ini yang melayani produksi (dijalankan dari branch
`main`, lihat `Dockerfile` dan `docker-compose/docker-compose.main.yml`).

| Direktori | Peran | Nasib di rewrite |
|---|---|---|
| `medium-parser/` | fetch GraphQL + parse HTML Medium | → `medium-doc` + `medium-render` + `medium-client` |
| `database-lib/` | kontrak Postgres | → `freedium-cache` |
| `web/` | axum-nya masih FastAPI; `web/server/templates/` | → `freedium-web`; templates dipakai apa adanya |
| `rl_string_helper/` | splicing string + `string_pos_matrix` | **dihapus**, diganti IR |
| `freedium-library/` | refactor DI Python yang sedang jalan | status perlu konfirmasi, lihat plan §6 |
| `sitemap-generator/` | script terpisah | tetap Python |
| `tests/`, `test_lab/` | test + playground | referensi semantik |

Dua hal yang perlu diingat saat membaca kode di sini:

1. **Jangan replikasi bug-nya.** `RUST_REWRITE_PLAN.md` §7 mendaftar perilaku yang
   sengaja tidak ikut pindah (debug write ke `/app/web/sidufh.json`, `ORDER BY
   RANDOM()`, dsb).
2. **Path container sengaja tidak berubah.** `Dockerfile` meng-`COPY` dari
   `legacy/...` tapi menaruhnya di `/app/...` yang sama seperti sebelumnya, supaya
   referensi absolut di dalam kode (mis. `/app/web/sidufh.json`) dan bind mount
   compose tetap valid.

`xtask/difftest/` dan `xtask/spike-impersonate/` memakai sebagian file di sini
sebagai oracle pembanding Python-vs-Rust — itu sebabnya kode ini masih
dipertahankan utuh, bukan diarsipkan.
