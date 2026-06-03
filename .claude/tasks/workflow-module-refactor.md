# Refactor planner/runner → workflow module

**Status:** done — Phase A + B + C complete (rename + facade + supervisor split + tracing dir + docs)
**Priority:** P1
**Area:** packages/agent
**Created:** 2026-06-03

## Context

`packages/agent/src/planner/` содержит две половинки одного механизма
(LLM компилит сигнал в DSL → runtime его исполняет), но названы как
два независимых модуля. Supervisor знает про оба напрямую, плюс
держит fallback session + recovery — выросло до 375 LOC и слои
перепутаны.

После шести итераций plan-then-execute мы устаканились на названии
«dynamic workflow». Текущие имена (`planner`, `runner`, `Plan`) —
артефакт ранних обсуждений; пора привести код к финальной
терминологии и разобрать supervisor.

Структура сейчас (5.4k LOC, agent):

```
src/
├── supervisor.ts (375)        main loop + fallback + recovery
├── engine.ts (226)
├── session.ts (570)           ReAct loop (legacy / llm_agent body)
├── session-context.ts (92)
├── synthetic-tools.ts (264)   invoke_sub_agent, set_memory
├── planner/                   dsl, planner, runner, substitute + tests
└── …
```

## Acceptance

Готово когда:

1. Директория `planner/` переименована в `workflow/`, файлы:
   - `planner.ts` → `compile.ts` (export `createCompiler`)
   - `runner.ts` → `execute.ts` (export `createExecutor`)
   - `substitute.ts` → `variables.ts`
   - `dsl.ts` остаётся
   - тесты переименованы вслед
2. Тип `Plan` (и связанные `PlanSchema`, `parsePlan`, `formatPlanErrors`,
   `planToJsonSchema`, `ToolStep`, `LlmComposeStep` …) переименованы в
   `Workflow` / `WorkflowSchema` / `parseWorkflow` / etc.
3. `skills.default/planner.md` тоже обновлён под новую терминологию
   (упоминания «Plan» в промпте → «Workflow»). Сам файл планировщика
   пока не переименовываем (signal source `planner` нет, имя skill'а
   — внутренняя деталь).
4. Новый `workflow/index.ts` экспортит фасад `createWorkflow({engine,
   mcpTools, knownSkills, readSkill, maxAttempts?})` с методом
   `runForSignal(signal, envData, parentTrace, signalLabel)`. Supervisor
   импортит **только** этот фасад, не `compile` / `execute` напрямую.
5. Fallback логика вынесена из `supervisor.ts` в
   `supervisor/fallback.ts` (`runFallbackSession`,
   `reportRunnerFailureToUser`, `reportFailureToUser`, `spawnRecovery`).
   `supervisor.ts` → `supervisor/main.ts` стал ~120 LOC, в шапке
   3-строчная блок-схема «signal → workflow → [fallback / recovery]».
6. `pnpm typecheck` зелёный.
7. Тесты зелёные (`workflow/*.test.ts` адаптированы под новые имена).
8. Trace span names в Langfuse остаются `planner` и `runner` — иначе
   continuity со старыми трейсами теряется. Это явно прокомментировано
   в `compile.ts` / `execute.ts`.
9. `CLAUDE.md` раздел Layout обновлён под новую раскладку.

Опциональная Phase C (отдельный PR):
- `tracing.ts` + `tracing-langfuse.ts` → `tracing/{index,langfuse}.ts`.
- Комментарии-роли в `session.ts` («agentic body for `llm_agent` step
  и supervisor fallback path») и `synthetic-tools.ts` («live только
  ради legacy session — workflow не подгружает»).

## Plan

Три PR'а, последовательно. Каждый mechanical, легко откатить.

### Phase A — переименование + фасад ✅ DONE

- `git mv packages/agent/src/planner packages/agent/src/workflow`
- В пределах `workflow/`:
  - `planner.ts` → `compile.ts`. `createPlanner`/`Planner`/`PlannerDeps`
    /`PlannerResult`/`PlannerFailureReason`/`PlanRequest` →
    `createCompiler`/`Compiler`/`CompilerDeps`/`CompilerResult`/
    `CompilerFailureReason`/`CompileRequest`.
  - `runner.ts` → `execute.ts`. `createRunner`/`Runner`/`RunnerDeps`/
    `RunContext`/`RunResult`/`RunFailureReason` →
    `createExecutor`/`Executor`/`ExecutorDeps`/`ExecContext`/`ExecResult`/
    `ExecFailureReason`.
  - `substitute.ts` → `variables.ts`. Экспорт остаётся
    (`createStore`/`substitute`/`MissingBindingError`/
    `DuplicateBindingError`).
  - `dsl.ts`: `Plan` → `Workflow`, `PlanSchema` → `WorkflowSchema`,
    `createPlanSchema` → `createWorkflowSchema`, `parsePlan` →
    `parseWorkflow`, `formatPlanErrors` → `formatWorkflowErrors`,
    `planToJsonSchema` → `workflowToJsonSchema`. Step-типы остаются
    (`ToolStep`, `LlmComposeStep`, `LlmAgentStep`, `ParallelStep`,
    `TerminalStep`) — они и так про шаги workflow.
- Промпт планировщика: `skills.default/planner.md` — заменить
  «plan» → «workflow» в инструкциях и примерах. Слово «steps»
  оставить (оно про шаги).
- Новый `workflow/index.ts`:
  ```ts
  export function createWorkflow(deps: {
    engine, mcpTools, knownSkills, readSkill, maxAttempts?
  }): { runForSignal(signal, envData, parentTrace, signalLabel): Promise<…> }
  ```
  Внутри композит из compiler + executor. Возвращает дискриминированный
  union: `{ok: true}` | `{ok: false, stage: 'compile'|'execute', …}`.
- Supervisor обновлён: импорт только `createWorkflow`. Никаких
  `createCompiler` / `createExecutor` в `supervisor.ts`.
- Span names: внутри `compile.ts` / `execute.ts` оставляем
  `parentTrace.span({name: "planner"})` и `name: "runner"` — комментарий
  «name kept for trace continuity with pre-rename history».
- Тесты переименованы: `compile.test.ts`, `execute.test.ts`,
  `variables.test.ts`, `dsl.test.ts`. Test imports обновляются под новые
  имена. Прогнать `pnpm test packages/agent`.

### Phase B — supervisor split ✅ DONE

- Создать директорию `packages/agent/src/supervisor/`.
- `supervisor.ts` → `supervisor/main.ts`. Оставить только: `main()`,
  poll loop, `runSignal()` тонкий (gather env → `workflow.runForSignal`
  → если failed → `fallback.handle(...)`).
- `supervisor/fallback.ts`: переезжают `runFallbackSession`,
  `reportRunnerFailureToUser`, `reportFailureToUser`, `spawnRecovery`,
  `buildPromptPrefix`. Экспорт `createFallback({engine, readSkill})`
  с методом `handle(signal, envData, failure, trace)`.
- `package.json` script `agent:start`: если ссылается на
  `dist/supervisor.js` — поправить на `dist/supervisor/main.js`.
- Шапка `main.ts`: 3-строчный поток (signal → workflow → fallback /
  recovery).

### Phase C — мелочи (отдельный PR, опционально) ✅ DONE

- `tracing.ts` + `tracing-langfuse.ts` → `tracing/{index,langfuse}.ts`.
  Все импорты `from "./tracing"` остаются (resolve на `index.ts`).
- Комментарии-роли в `session.ts` и `synthetic-tools.ts`.
- `CLAUDE.md` Layout block обновить.

## Notes

- **Что НЕ ломаем:**
  - `EngineSurface` / `SubSessionHandle` интерфейсы — структурные
    контракты для тестов, имена методов не трогаем.
  - Trace span names (`planner`, `runner`) — continuity со старыми
    Langfuse трейсами важнее consistency имён.
  - Skill files на диске (`skills.default/*.md`) — пользовательские
    артефакты, имена остаются.
- **Что осознанно НЕ делаем сейчас:**
  - Бить `execute.ts` на per-step файлы. 494 LOC + один switch + тесты
    рядом — добавит indirection без пользы.
  - Менять signal source naming (`source=telegram`, etc).
- **Risk:** import paths меняются в куче файлов сразу. Ловить через
  `pnpm typecheck` и `pnpm test packages/agent` после каждой фазы.
  Делать через `git mv` чтобы blame не терялся.
- **Деплой:** после каждой фазы — `git push` + `docker compose up -d
  --build agent` на droplet'е. Поведение не меняется, но catch'ит
  packaging baked-in paths если такие есть.
