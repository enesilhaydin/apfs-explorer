# context-mode — MANDATORY routing rules (tam metin)

> CLAUDE.md'de özeti var. Bu dosya tam kuraldır. **Opsiyonel değil** — context
> window'u flooding'den korur. Tek yönlendirilmemiş komut 56 KB dump edip
> session'ı yakabilir.

## BLOKE komutlar — denENMEZ

### curl / wget — BLOKE
İçinde `curl` veya `wget` geçen Bash komutu yakalanır, hata ile değiştirilir.
Tekrar deneme. Yerine:
- `ctx_fetch_and_index(url, source)` — web sayfası getir+indeksle
- `ctx_execute(language:"javascript", code:"const r = await fetch(...)")`

### Inline HTTP — BLOKE
`fetch('http`, `requests.get(`, `requests.post(`, `http.get(`,
`http.request(` içeren Bash komutu yakalanır. Yerine `ctx_execute(language,
code)` — sandbox'ta çalışır, sadece stdout context'e girer.

### WebFetch — BLOKE
WebFetch tamamen reddedilir. URL çıkarılır → `ctx_fetch_and_index(url, source)`
sonra `ctx_search(queries)`.

## YÖNLENDİRİLEN tool'lar

### Bash (>20 satır çıktı)
Bash SADECE: `git`, `mkdir`, `rm`, `mv`, `cd`, `ls`, `npm install`,
`pip install` ve kısa-çıktılı komutlar. Gerisi:
- `ctx_batch_execute(commands, queries)` — çok komut + arama TEK çağrı
- `ctx_execute(language:"shell", code:"...")` — sandbox, sadece stdout

### Read (analiz için)
Edit edeceksen → Read doğru (Edit içerik ister). Analiz/keşif/özet için →
`ctx_execute_file(path, language, code)`; sadece printed özet context'e girer.

### Grep (büyük sonuç)
`ctx_execute(language:"shell", code:"grep ...")` — sadece özet context'e girer.

## Tool seçim hiyerarşisi

1. **GATHER:** `ctx_batch_execute(commands, queries)` — birincil. 30+ çağrıyı
   tek round-trip'e indirir.
2. **FOLLOW-UP:** `ctx_search(queries:[...])` — tüm sorular tek array.
3. **PROCESSING:** `ctx_execute` / `ctx_execute_file` — sandbox, stdout only.
4. **WEB:** `ctx_fetch_and_index(url, source)` → `ctx_search(queries)`.
5. **INDEX:** `ctx_index(content, source)` — FTS5 bilgi tabanı.

## Subagent routing

Agent/Task spawn'da routing bloğu otomatik enjekte edilir; Bash-tipi subagent'lar
general-purpose'a yükseltilir (MCP erişimi). Manuel talimat gerekmez.

## Output kısıtları

- Yanıtlar < 500 kelime.
- Artefaktlar (kod/config/PRD) **dosyaya** yazılır, inline değil. Dönen: dosya
  yolu + 1 satır açıklama.
- İndekslerken açıklayıcı `source` etiketi (başkası `ctx_search(source:"...")`
  yapabilsin).

## ctx komutları

| Komut | Aksiyon |
|---|---|
| `ctx stats` | `ctx_stats` MCP tool'unu çağır, çıktıyı aynen göster |
| `ctx doctor` | `ctx_doctor` MCP tool, dönen shell komutunu çalıştır, checklist |
| `ctx upgrade` | `ctx_upgrade` MCP tool, dönen shell komutunu çalıştır, checklist |
