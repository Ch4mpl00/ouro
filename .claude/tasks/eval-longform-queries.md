# Long-form eval queries — draft for review

**Status:** review
**Priority:** P2
**Area:** evals / retrieval
**Created:** 2026-06-02

## Why

`queries.jsonl` сейчас почти целиком покрывает только channel (TG) посты — 18 из 19 запросов имеют gold исключительно из коротких сообщений. Поиск по длинным статьям (141 habr + 80 hackernews = 221 рядов в корпусе) **не оценивается**. Это значит:

- Эффекты усечения текста на 6000 символов (`EVAL_MAX_CHARS`) не видны метриками.
- Поведение на длинных контекстах (статья на 90K, где тема упомянута в одном абзаце посередине) — невидимо.
- Cross-lingual ретривал (англоязычные HN-обсуждения по русскоязычному запросу) — невидимо.

## Что сделано

Запущен workflow `wfd0go8cq` (16 агентов, 837K токенов, 4.5 мин). 15 агентов параллельно прочитали свои батчи (~15 статей каждый) и для каждой статьи выдали `{main_themes, adjacent_topics, notable_aspect}`. Один синтез-агент склеил темы в кластеры (≥2 статей в каждом) и предложил 15 запросов в стиле существующего `queries.jsonl`.

Все 15 запросов прошли валидацию: gold ids существуют в корпусе и все принадлежат `habr`/`hackernews`.

## Кандидаты на добавление в queries.jsonl

Ниже — отсортировано по subjective quality (мой грубый ранкинг). Ревьюить и решать что брать.

### A. Чистые кластеры — высокая уверенность

**Q1. SDR на Simulink и Zynq** — 4 статьи серии одного автора
```json
{"query": "посмотри статьи про SDR на Simulink и Zynq",
 "reformulation": "SDR software defined radio Simulink Zynq FPGA MathWorks HDL coder model-based design RF signal processing embedded hardware",
 "gold": [1926, 2023, 2024, 2025], "acceptable": []}
```

**Q2. Surface Laptop Ultra / RTX Spark** — 3 статьи об одном анонсе с разных углов, одна по-русски
```json
{"query": "что там новое от Microsoft и NVIDIA с локальным ИИ на ноуте?",
 "reformulation": "Microsoft NVIDIA local AI laptop NPU on-device inference edge AI Windows Copilot+ neural processing unit local LLM hardware acceleration notebook GPU",
 "gold": [1494, 1497, 2104], "acceptable": []}
```
Тестирует cross-lingual: gold содержит англ. HN и русский habr-перевод.

**Q3. AI-RPG guard hallucinations** — Стирая Грань
```json
{"query": "как борются с галлюцинациями LLM-мастера в текстовых RPG?",
 "reformulation": "LLM hallucinations text RPG game master AI dungeon grounding facts consistency narrative control retrieval augmented generation mitigation techniques",
 "gold": [1924, 3880], "acceptable": [4205]}
```
ВАЖНО: 1924 и 3880 имеют одинаковый title — возможно реальный дубль в корпусе. Проверить: если так, dedup на стадии ретривала уберёт один → recall_at_K никогда не достигнет 1.0. Решение: либо убрать один из gold, либо пометить пару дублями в фикстуре.

**Q4. Meta AI support → Instagram hack**
```json
{"query": "что писали про взлом инстаграма через AI-поддержку Меты?",
 "reformulation": "Instagram hack exploit Meta AI support social engineering vulnerability account takeover phishing customer support bypass взлом аккаунта",
 "gold": [1373, 1980], "acceptable": [824]}
```

**Q5. Hiring threads HN**
```json
{"query": "ask hn who is hiring",
 "reformulation": "Ask HN who is hiring Hacker News jobs thread monthly hiring remote positions tech companies",
 "gold": [1273, 1485], "acceptable": [17, 29]}
```
В стиле остальных queries это аномалия — все остальные по-русски разговорные. Альтернатива: `query: "что там в hacker news про найм этим месяцем?"`.

### B. Хорошие, но требуют редактирования

**Q6. Claude Code / .claude config** — 5 свежих habr-статей по теме
```json
{"query": "что пишут про настройку .claude и Claude Code в проде?",
 "reformulation": "Claude Code production setup configuration .claude directory CLAUDE.md settings hooks permissions MCP deployment workflow AI coding assistant",
 "gold": [2118, 3743, 3844, 4203, 4298], "acceptable": [1404, 3931, 4397, 4118]}
```
Пересекается с Q7 на id=2118 — это ок как разные ракурсы.

**Q7. AI replacing manual coding**
```json
{"query": "кто-то писал как перестал писать код руками с ИИ?",
 "reformulation": "vibe coding AI-assisted development no manual coding LLM code generation workflow автоматизация разработки cursor copilot agent programming без ручного кода",
 "gold": [2118, 3935, 4118, 4397], "acceptable": [2989, 4300, 4487]}
```

**Q8. .NET no-GC optimization** — серия одного автора
```json
{"query": "оптимизация .NET для игр без GC пауз",
 "reformulation": ".NET game optimization garbage collector GC pauses latency NativeAOT Span memory allocation pooling Unity server-side real-time performance",
 "gold": [30, 981, 1210], "acceptable": [4476]}
```

**Q9. AI scrapers → open source pullback**
```json
{"query": "разработчики закрывают опенсорс из-за ИИ-скраперов?",
 "reformulation": "open source developers closing repositories AI scrapers training data crawlers licensing protest terms of service LLM training opt-out GitHub",
 "gold": [812, 815], "acceptable": [4392]}
```
Запрос намеренно расходится с формулировкой в статьях — тест семантической генерализации.

**Q10. Telegram bots architecture**
```json
{"query": "что писали про архитектуру telegram-ботов?",
 "reformulation": "Telegram bot architecture design patterns webhook long polling bot API scalability state management session handlers framework aiogram python node",
 "gold": [19, 3937, 4120], "acceptable": [4205, 1, 4118]}
```
acceptable [1, 4118] спорные — статьи о других вещах, не о tg-архитектуре.

**Q11. Local LLM on weak/old hardware**
```json
{"query": "кто запускал большие модели на слабом железе?",
 "reformulation": "large language model weak hardware low VRAM CPU inference quantization GGUF llama.cpp 4-bit 8-bit small GPU budget hardware LLM local inference",
 "gold": [804, 825], "acceptable": [1494, 1497, 2104, 18]}
```

**Q12. Philosophy of AI / interpretability**
```json
{"query": "что-то философское про интерпретируемость ИИ и сознание?",
 "reformulation": "AI interpretability consciousness philosophy mechanistic interpretability explainability sentience machine mind black box neural network understanding",
 "gold": [2, 2814], "acceptable": [3107, 896, 4110]}
```

### C. Слабые — может выкинуть

**Q13. AI in education** — кластер натянут: gold смешивает «ИИ ломает образование» с «pipeline vs автономные агенты» (последнее не про образование)
```json
{"query": "что говорят про ИИ в образовании и письменных работах?",
 "reformulation": "AI education academic writing essays students cheating detection LLM plagiarism schools universities письменные работы образование искусственный интеллект",
 "gold": [4391, 4489, 4492], "acceptable": [983, 1404, 810]}
```

**Q14. Russian AI services without VPN** — кластер ещё хуже: запрос про «доступность из России без VPN», а gold — статьи про генерацию видео/обложек/сайтов; «без VPN» в них только вскользь
```json
{"query": "есть что про нейронки доступные из России без VPN?",
 "reformulation": "нейросети доступные Россия без VPN AI сервисы незаблокированные ChatGPT аналоги российский рынок языковые модели доступность санкции",
 "gold": [983, 1430, 1925, 4207], "acceptable": [3939, 2119]}
```

**Q15. RAG and retrieval evolution**
```json
{"query": "посмотри про RAG и поиск похожих документов",
 "reformulation": "RAG retrieval augmented generation semantic search vector similarity embeddings nearest neighbor document retrieval chunking reranking pgvector",
 "gold": [3647, 3845, 3933], "acceptable": [8, 4398]}
```
Связь между статьями в gold натянута (More Like This / контекст не заменяет retrieval / few-shot via MMR — три разные подтемы под одним запросом). Можно либо взять, либо разбить на 2 более узких.

## Acceptance — что сделать

1. Пройтись по A-блоку, добавить как `q-020..q-024` (5 шт.) в `queries.jsonl`.
2. Из B взять Q6/Q7/Q8/Q9/Q11/Q12 → `q-025..q-030` (6 шт.). Q10 — либо урезать acceptable, либо тоже взять.
3. C-блок: либо переформулировать запросы под реальный gold, либо выкинуть.
4. Для Q3 (1924/3880) разобраться с дублем — `pnpm eval:inspect --qids q-XXX` после добавления покажет, удаляет ли их dedup на retrieval-стадии.
5. Гонять `pnpm eval:run --config baseline` и `baseline-dedup-003`. Смотреть на падение P@5/R@10 — это и есть наша «слепая зона» по длинным текстам, теперь количественно.
6. Если retrieval на длинных текстах окажется намного хуже channel'ов — это сигнал к тюнингу: длина чанка, отдельная индексация хедера-vs-тела, MMR, hybrid BM25+vec. См. также [[eval-llm-judge]].

## Notes

- Reformulations пересоставлены отдельным cold-агентом (sonnet) без доступа к содержимому статей — только по тексту самого `query`. Это важно: первая версия reformulations была сгенерирована тем же агентом, что видел статьи, и поэтому содержала имена конкретных проектов/продуктов/людей (Kefir compiler, movwin, Chalmers, Egan, Telethon-aiopayme), что превращает «реформуляцию» в утечку gold-сигнала и завышает метрики. Новые reformulations расширяют только то, что следует из самого запроса (синонимы, многоязычные варианты, доменный жаргон).
- Workflow артефакты: `/tmp/eval-long-batches/batch-{00..14}.jsonl` (per-batch slices) и transcript в `~/.claude/projects/-Users-dimas-Code-mcp-tools/.../wf_8d8d7b43-ec2/`. Можно удалить когда черновик отревьюен.
- 1406 строк корпуса, из них 221 long-form. После добавления этих 15 запросов имеем ~17 LF-таргетных против 18 channel-таргетных — баланс восстановлен.
- В Q3 видна закладка для проверки dedup: одинаковый title в двух id. Это полезный edge case в фикстуре, не bug.
