`arm.gba`, `thumb.gba` и `bios.gba` — тестовые ROM'ы из https://github.com/jsmolka/gba-tests (MIT, © Julian Smolka).
Используются в `src/gba.rs` как регрессионные тесты CPU.

`psg_square.gba` — крошечный ROM, собранный вручную в этом проекте: включает звук и запускает
прямоугольный канал 1 (используется тестом звука в `src/gba.rs`).
