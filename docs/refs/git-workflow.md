# Git Workflow

Trunk-based + kısa ömürlü feature branch (solo dev, disiplinli).

## Akış (her yeni iş için)

```
1. main sync       git fetch origin && git rebase origin/main
2. Branch          git checkout -b <prefix>/<kebab-ad>
3. Commit (atomik) git add <dosya>   (ASLA -A/.)  + git commit -m "..."
4. Sync (uzunsa)   git rebase origin/main
5. Yeşil           cargo fmt --check && cargo clippy -D warnings && cargo test
6. Push            git push -u origin <branch>
7. PR              gh pr create --base main
8. Self-review + code-reviewer agent
9. Merge           gh pr merge --squash --delete-branch
10. Cleanup        git checkout main && git pull --rebase && git branch -d <branch>
```

## Branch isimlendirme (zorunlu)

| Prefix | Anlam | Örnek |
|---|---|---|
| `feat/` | Yeni feature | `feat/apfs-core-superblock` |
| `fix/` | Bug fix | `fix/btree-truncated-image-panic` |
| `refactor/` | Davranış aynı | `refactor/blockdevice-trait` |
| `docs/` | Sadece doküman | `docs/refs-refresh` |
| `chore/` | Build/deps/release | `chore/winfsp-pin-bump` |
| `test/` | Sadece test/fixture | `test/lzfse-fixtures` |
| `archive/` | Eski work yedek | `archive/spike-pure-rust-2026-05` |

## Kurallar

- **`main` her zaman deployable** — kırık commit girmez.
- **Feature branch ömrü ≤ 3 gün** — uzun = sık `git rebase origin/main`.
- **Hot-fix istisnası:** prod crash/security → doğrudan `main` tek commit + kısa açıklama.
- **Merge:** `squash + delete-branch` (lineer history); merge commit yok.
- **Branch silme:** PR merge sonrası HEMEN sil.
- **`git add -A` / `git add .` YASAK** — root `node_modules/`/`target/` riski,
  daima dosya ismi.
- **`main`'e force-push YASAK** — feature branch'te rebase sonrası OK.
- **Yedek branch:** sadece `archive/` prefix, 30 gün sonra `git branch -D`.

## İlk kurulum (M0)

- `git init` (zaten repo), `main` default.
- `.gitignore`: `/target`, `**/*.rs.bk`, IDE, `*.dmg` (fixture hariç),
  `installer/build/`.
- `.gitattributes` + Git LFS: `tests/fixtures/** filter=lfs diff=lfs merge=lfs`.
- `LICENSE` = GPLv3. İlk commit = M0 iskeleti (her crate'in `CLAUDE.md`'i dahil).
- Commit mesajı sonu (proje konvansiyonu):
  `Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>`
