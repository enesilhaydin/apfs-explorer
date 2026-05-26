# APFS Spec Kaynakları (BİRİNCİL)

Parser'ı **sıfırdan biz yazıyoruz**. On-disk yapı, alan offset'leri, B-tree
düzeni, sıkıştırma/şifreleme detayları için **tek doğruluk kaynağı** Apple "Apple
File System Reference"tır. Bize iki kanaldan açık:

## 1. Context7 — `pradorocchi/apfs-filesystem-full-reference`

Tam referansın indekslenmiş hali. Bir yapıyı kodlamadan önce sorgula.

- Tool: `mcp__plugin_context7_context7__resolve-library-id` → sonra
  `query-docs` (library id: `pradorocchi/apfs-filesystem-full-reference`).
- Kullanım örneği soruları:
  - "nx_superblock field layout and magic"
  - "object map btree omap_phys structure"
  - "j_drec_val_t directory record value fields"
  - "file extent fext_tree / file extent records"
  - "decmpfs header / compression method constants (ZLIB LZVN LZFSE)"
  - "fusion / wbc structures", "snapshot metadata xid"
  - "encryption keybag / wrapped VEK / kek"

## 2. NotebookLM — kullanıcının yüklediği APFS notebook (soru-cevap)

Aynı doküman + serbest soru-cevap. Belirsiz/derin noktada (ör. bir struct'ın
sürüm farkı, bir flag'in semantiği) buraya sor.

Akış (server talimatı):
1. `get_health` → `authenticated=false` ise `setup_auth` (kullanıcı bir kez
   tarayıcıdan giriş yapar).
2. Kullanıcı notebook share-URL'sini verir → `add_notebook` →
   `select_notebook`.
3. `ask_question` → dönen `session_id`'yi sakla, takip sorularında **aynı
   session_id**'yi geçir (RAG bağlamı keskinleşir).

> **KURULDU (2026-05-17):** notebook kayıtlı ve aktif.
> - id: `apfs-reference-apple-file-syst`
> - url: `https://notebooklm.google.com/notebook/8df08de6-02ed-47c1-8a63-3693df7eebc0`
> - içerik: Apple resmi ~180 sayfalık "Apple File System Reference".
> Yeni session'da: `get_health` → `authenticated=false` ise `setup_auth`,
> sonra bu id zaten aktif. `ask_question` ile sor, `session_id`'yi sakla.

## 3. Apple resmi PDF (doğrudan indirme — kullanıcı onayı)

Gerekirse Apple "Apple File System Reference" PDF'i doğrudan indirilip
indekslenir (curl/wget BLOKE → `ctx_fetch_and_index`):
- URL: `https://developer.apple.com/support/downloads/Apple-File-System-Reference.pdf`
- Önceki araştırmada zaten indekslendi → `ctx_search(source:"apfs-lit:spec")`
  ile sorgulanabilir. Tazelemek için `ctx_fetch_and_index(url, "apfs-lit:spec")`.

Üçlü kaynak (Context7 + NotebookLM + indexed PDF) çapraz doğrulanır;
çelişki olursa NotebookLM'e netleştirici soru sorulur.

## Çalışma kuralı

- Bir APFS struct/alan/algoritma yazmadan önce **önce Context7'den teyit et**;
  belirsizlik varsa NotebookLM'e sor. Bellekten/varsayımdan struct yazma.
- Spec'ten çıkardığın her non-trivial yapı için kod yorumunda kaynağı belirt
  (ör. `// APFS Ref: nx_superblock, see docs/refs/apfs-spec-sources.md`).
- Spec'te eksik/az dokümante alan (tarihsel olarak LZFSE iç codec'i, bazı Fusion
  detayları): *referans* olarak apfs-fuse / linux-apfs-rw kaynağına bakılabilir
  (kod kopyalanmaz; GPLv3 uyumlu, sadece doğrulama amaçlı).
- Çıkardığın yapıları FTS bilgi tabanına indeksle: `ctx_index(content,
  source:"apfs-spec:<konu>")` → sonra `ctx_search(source:"apfs-spec")`.
