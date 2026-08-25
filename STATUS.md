# STATUS

Единственный источник истины по статусу проекта (стандарт Knowledge/System).

## Current State

**Стадия:** Active Development
**Версия:** 0.4.0 (в разработке; v0.1.0 тегирован, v0.3 смонтирован и обкатан руками)
**Дата обновления:** 2026-08-26 (ночная смена)

### Готово

- **v0.1**: CLI ls/pull/push/mkdir/rm/mv/info/doctor/bench + device-actor.
- **v0.2**: bundle-mode — 500 файлов 21.95s → 0.09s (**233×**).
- **v0.3**: Finder-mount через NFSv3 loopback, read-only, проверено руками.
- **v0.4 (ночь)**: том ЗАПИСЫВАЕМЫЙ — write-back staging по ADR-004
  (write→stage, COMMIT→delete+upload, flushed_dev перепривязывает fh);
  хэндл-мутации в ядре; Disconnected-классификация; patient connect;
  connect_first перебирает кандидатов (урок USB-UART на шине);
  root-free e2e-скрипт записи.

### Ожидает устройства

- Прогон `scripts/e2e-write-test.sh` (телефон ушёл с шины из-за OTA;
  после загрузки — режим «Передача файлов» + разрешить доступ к данным).

### Дальше

- v0.5: атомарный flush (upload-new → rename), авто-reconnect актора,
  xattr-заглушки через NFSv3? (нет — только при переходе на v4), Homebrew tap.

## Risks

| Риск | Митигция |
|---|---|
| fernfs молод (для v0.3) | вендорим в workspace, патчим локально |
| Нестабильные замеры между запусками | методика и числа зафиксированы в docs/benchmarks |
