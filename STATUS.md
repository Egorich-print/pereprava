# STATUS

Единственный источник истины по статусу проекта (стандарт Knowledge/System).

## Current State

**Стадия:** Prototype → Active Development
**Версия:** 0.1.0 (в разработке)
**Дата обновления:** 2026-08-25

### Готово

- Скаффолд workspace (edition 2024, rust 1.98), CI, доки, ADR-000..003.
- Изучен и зафиксирован API mtp-rs 0.30 (см. docs/status/mtp-rs-api-notes.md).
- Симлинк в хабе: `New OpenCode Project/projects/pereprava`.

### В работе

- Фаза 2: core (device-actor, кэш) + CLI (ls/pull/push/mkdir/rm/mv/info/doctor).

### Не начато

- v0.1.x bench + baseline-замеры на Nothing Phone (2).
- v0.2 компрессия (bundle-mode / ADB-zstd по автодетекту).
- v0.3 NFS-mount (вендоренный fernfs).
- Реестр проектов/Obsidian/showcase (после первого тега).

## Решения

| Решение | Обоснование |
|---|---|
| mtp-rs вместо libmtp | чистый async Rust, без FFI, быстрее в 1–4× (ADR-001) |
| NFSv3 loopback вместо macFUSE/FUSE-T/FSKit | без kext и системных зависимостей (ADR-002) |
| Компрессия только по данным бенчмарка | ≥25% выигрыша на реальном профиле, иначе cut (ADR-003) |
| MIT, clean-room rewrite | оригинал GPLv2 не копировался (ADR-000) |

## Risks

| Риск | Митигция |
|---|---|
| ptpcamerad перехватывает устройство на macOS | `doctor` + задокументированный фикс |
| fernfs молод (0.1.5) | вендорим в workspace, патчим локально |
| Один USB-сеанс на устройство | весь трафик через один device-actor |
