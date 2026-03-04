# Public Benchmark Details

This report is a reproducible, fully public A/B benchmark of `budi` with exact repos, prompts,
session-level hook evidence, responses, and judge rationales.

- Generated (UTC): 2026-03-04T15:58:25.542496+00:00
- Source runner: `scripts/ab_benchmark_runner.py`
- Report generator: `scripts/generate_public_benchmark_details.py`
- Prompt-set fingerprint: `6c3ac7531b6aadddb020036fbbca250e548d3c10e2273e7497525310a3dc963f`
- Cases: 9 (3 repos x 3 prompts)

## Repositories

| Repo | URL | Commit |
| --- | --- | --- |
| React | [https://github.com/facebook/react](https://github.com/facebook/react) | `3bc2d41` |
| Flask | [https://github.com/pallets/flask](https://github.com/pallets/flask) | `c34d6e8` |
| Express | [https://github.com/expressjs/express](https://github.com/expressjs/express) | `6c4249f` |

## Topline

| Metric | Result |
| --- | --- |
| API speed (aggregate) | **32.16% faster** with `budi` |
| End-to-end speed (aggregate) | **31.48% faster** with `budi` |
| Total cost (aggregate) | **18.42% lower** with `budi` |
| Judge wins | `with_budi` 4 / `no_budi` 5 / tie 0 |
| Avg quality delta | `-0.37` (`with_budi - no_budi`) |
| Avg grounding delta | `+0.00` (`with_budi - no_budi`) |
| Hook injection success | `9/9` rows had `reason=ok` |
| Avg injected context chars | `10070` |

```mermaid
xychart-beta
    title "Aggregate improvement with budi (higher is better)"
    x-axis ["API speed", "Wall speed", "Cost reduction"]
    y-axis "Percent" 0 --> 37
    bar [32.16, 31.48, 18.42]
```

```mermaid
pie showData
    title Judge winner split
    "with_budi" : 4
    "no_budi" : 5
    "tie" : 0
```

## Prompts

1. Give exactly 5 bullets describing repository architecture. Each bullet must include an exact file path. Keep total answer under 120 words. Do not edit files.
2. Name up to 5 exact file paths where runtime configuration or environment variables are loaded/validated. Use only files you can verify. Keep concise. Do not edit files.
3. Trace one concrete request/data flow from an entrypoint to output with exact file paths and function names in no more than 8 lines. Stop after one complete path. Do not edit files.

## Repo Summary

| Repo | API delta (with-no) | Wall delta (with-no) | Cost delta (with-no) | Judge wins (with/no/tie) |
| --- | ---: | ---: | ---: | --- |
| React | -46.42% | -45.20% | -31.16% | 1/2/0 |
| Flask | -42.71% | -40.80% | -29.52% | 2/1/0 |
| Express | -4.65% | -5.25% | +7.72% | 1/2/0 |

## Full Case Evidence

Each case includes: prompt, hook injection trace (`with_budi`), both final model responses,
interaction counts, and judge justification.

### React

<details><summary>React · Prompt 1 · winner=with_budi</summary>

- Prompt: Give exactly 5 bullets describing repository architecture. Each bullet must include an exact file path. Keep total answer under 120 words. Do not edit files.
- Interactions: no_budi=4 / with_budi=1
- API duration ms: no_budi=12527 / with_budi=5054
- Cost USD: no_budi=0.06059 / with_budi=0.03588
- Hook output: success=True reason=ok context_chars=12000 retry=False

#### What budi injected

```text
[budi deterministic context]
rules:
- Use only file paths shown in snippets for exact-path answers.
- If snippets support fewer files than requested, return fewer instead of guessing.
snippets:
### packages/react-native-renderer/src/legacy-events/forEachAccumulated.js:1-32 score=0.5499 reasons=lexical-hit,path-token-match,specific-path-hit,path-intent-boost,scope-miss,architecture-anchor-miss,clause-match
/**
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 *
 * @flow
 */

/**
 * @param {array} arr an "accumulation" of items which is either an Array or
 * a single item. Useful when paired with the `accumulate` module. This is a
 * simple utility that allows us to reason about a collection of items, but
 * handling the case when there is exactly one item (and we do not need to
 * allocate an array).
 * @param {function} cb Callback invoked with each element or a collection.
 * @param {?} [scope] Scope used as `this` in a callback.
 */
function forEachAccumulated<T>(
  arr: ?(Array<T> | T),
  cb: (elem: T) => void,
  scope: ?any,
) {
  if (Array.isArray(arr)) {
    // $FlowFixMe[incompatible-call] if `T` is an array, `cb` cannot be called
    arr.forEach(cb, scope);
  } else if (arr) {
    cb.call(scope, arr);
  }
}

export default forEachAccumulated;
### scripts/rollup/shims/facebook-www/ReactBrowserEventEmitter_DO_NOT_USE.js:10-12 score=0.5073
```

#### Final response (`no_budi`)

```text
- **Monorepo with 38+ packages** under `packages/` — each with its own `package.json` (e.g., `packages/react/package.json`, `packages/react-reconciler/package.json`).
- **React Compiler** lives in a separate sub-monorepo at `compiler/` with its own `compiler/CLAUDE.md` and toolchain.
- **Renderer abstraction** — platform-specific renderers (`packages/react-dom/`, `packages/react-native-renderer/`) are built on top of the shared `packages/react-reconciler/` package.
- **Server Components infrastructure** is split across `packages/react-server/`, `packages/react-client/`, and bundler-specific adapters like `packages/react-server-dom-webpack/`.
- **Build system** is configured at root via `babel.config.js` and versioning is centralized in `ReactVersions.js`.
```

#### Final response (`with_budi`)

```text
- **Core library entry point**: `packages/react/index.js` — exports the public React API (createElement, hooks, etc.)
- **Reconciler**: `packages/react-reconciler/src/ReactFiber.js` — implements the Fiber data structure powering React's reconciliation algorithm
- **DOM renderer**: `packages/react-dom/src/ReactDOM.js` — bridges the reconciler to browser DOM operations
- **Scheduler**: `packages/scheduler/src/Scheduler.js` — cooperative task scheduling used by the reconciler to prioritize work
- **Build system**: `scripts/rollup/build.js` — orchestrates Rollup-based builds across all packages, producing CJS/ESM/UMD bundles per environment (dev/prod/profiling)
```

#### Judge rationale

```text
Response B (with_budi) better satisfies the "exact file path" requirement by pointing to specific source files (e.g., packages/react-reconciler/src/ReactFiber.js, scripts/rollup/build.js) rather than directories or package.json files. Response A (no_budi) mostly cites directory-level paths (packages/react-dom/, packages/react-server/) which are not true file paths. Response B also covers more architecturally meaningful entry points (core API, reconciler/Fiber, DOM renderer, scheduler, build system) with tighter, more actionable descriptions. Both fit within 120 words. Response A's mention of ReactVersions.js without a full path and babel.config.js as the "build system" is less precise than B's scripts/rollup/build.js, which more accurately represents the actual build orchestration in React's codebase.
```

</details>

<details><summary>React · Prompt 2 · winner=no_budi</summary>

- Prompt: Name up to 5 exact file paths where runtime configuration or environment variables are loaded/validated. Use only files you can verify. Keep concise. Do not edit files.
- Interactions: no_budi=8 / with_budi=1
- API duration ms: no_budi=16206 / with_budi=9580
- Cost USD: no_budi=0.05752 / with_budi=0.04327
- Hook output: success=True reason=ok context_chars=12000 retry=False

#### What budi injected

```text
[budi deterministic context]
rules:
- Use only file paths shown in snippets for exact-path answers.
- If snippets support fewer files than requested, return fewer instead of guessing.
- For runtime config/env prompts, prioritize runtime loaders/validators over build-time config.
snippets:
### scripts/rollup/generate-inline-fizz-runtime.js:48-101 score=0.8732 reasons=lexical-hit,path-token-match,specific-path-hit,path-intent-boost,runtime-config-path-hit,clause-match
async function main() {
  const exportStatements = await Promise.all(
    config.map(async ({entry, exportName}) => {
      const fullEntryPath = instructionDir + '/' + entry;
      const compiler = new ClosureCompiler({
        entry_point: fullEntryPath,
        js: [
          require.resolve('./externs/closure-externs.js'),
          fullEntryPath,
          instructionDir + '/ReactDOMFizzInstructionSetShared.js',
        ],
        compilation_level: 'ADVANCED',
        language_in: 'ECMASCRIPT_2020',
        language_out: 'ECMASCRIPT5_STRICT',
        module_resolution: 'NODE',
        // This is necessary to prevent Closure from inlining a Promise polyfill
        rewrite_polyfills: false,
      });

      const code = await new Promise((resolve, reject) => {
        compiler.run((exitCode, stdOut, stdErr) => {
          if (exitCode !== 0) {
            reject(new Error(stdErr));
          } else {
            resolve(stdOut);
          }
        });
      });

      return `export const ${exportName} = ${
```

#### Final response (`no_budi`)

```text
Here are 5 verified file paths where runtime configuration or environment variables are loaded/validated:

1. **`scripts/rollup/build.js`** — loads `RELEASE_CHANNEL`, `NODE_ENV`, `CI_TOTAL`, `CI_INDEX` (line 31, 437, 867–869)
2. **`scripts/rollup/bundles.js`** — loads `RELEASE_CHANNEL` and `BUNDLES_FILTER` (lines 3, 1343–1349)
3. **`scripts/rollup/forks.js`** — loads `RELEASE_CHANNEL` to determine stable vs. experimental feature forks (line 20)
4. **`scripts/release/utils.js`** — reads `RELEASE_CHANNEL` to distinguish experimental releases (line 63)
5. **`scripts/rollup/utils.js`** — reads `HOME` for path resolution (line 79)
```

#### Final response (`with_budi`)

```text
Based on the snippets already provided in context, I can verify these files:

1. `packages/react-devtools-core/src/editor.js` — reads `process.env.REACT_EDITOR`, `process.env.VISUAL`, `process.env.EDITOR`
2. `packages/react-devtools-shared/src/config/DevToolsFeatureFlags.core-fb.js` — runtime feature flag config (FB build)
3. `packages/react-devtools-shared/src/config/DevToolsFeatureFlags.extension-oss.js` — runtime feature flag config (OSS extension)
4. `scripts/flow/environment.js` — declares global environment variables and runtime types
5. `scripts/shared/listChangedFiles.js` — reads runtime environment to detect git vs. sl repo
```

#### Judge rationale

```text
Response A (no_budi) cites five files in scripts/rollup/ and scripts/release/ that plausibly and commonly contain process.env reads in the React repo, and includes specific line numbers that suggest actual verification. The main weakness is that these are build-time scripts, not strictly "runtime" configuration for React itself, but they do unambiguously load/validate environment variables.

Response B (with_budi) has a significant credibility problem: it opens with "Based on the snippets already provided in context" when no such snippets were provided in the prompt — this is fabricated framing. Additionally, DevToolsFeatureFlags config files (items 2 and 3) are typically static constant exports, not env-var loading files. scripts/flow/environment.js is a Flow type declaration file, not a runtime env loader. The one plausible entry is packages/react-devtools-core/src/editor.js reading REACT_EDITOR/VISUAL/EDITOR, but the rest are questionable or wrong. Overall, Response B appears less verified and partly fabricated in its framing.
```

</details>

<details><summary>React · Prompt 3 · winner=no_budi</summary>

- Prompt: Trace one concrete request/data flow from an entrypoint to output with exact file paths and function names in no more than 8 lines. Stop after one complete path. Do not edit files.
- Interactions: no_budi=9 / with_budi=4
- API duration ms: no_budi=29673 / with_budi=16659
- Cost USD: no_budi=0.09500 / with_budi=0.06756
- Hook output: success=True reason=ok context_chars=12000 retry=False

#### What budi injected

```text
[budi deterministic context]
rules:
- Use only file paths shown in snippets for exact-path answers.
- If snippets support fewer files than requested, return fewer instead of guessing.
snippets:
### packages/react-devtools-shared/src/hooks/parseHookNames/loadSourceAndMetadata.js:181-260 score=0.6967 reasons=lexical-hit,path-token-match,specific-path-hit,path-intent-boost,scope-miss,clause-match
              () => decodeBase64String(trimmed),
            );

            const sourceMapJSON = withSyncPerfMeasurements(
              'JSON.parse(decoded)',
              () => JSON.parse(decoded),
            );

            if (__DEBUG__) {
              console.groupCollapsed(
                'extractAndLoadSourceMapJSON() Inline source map',
              );
              console.log(sourceMapJSON);
              console.groupEnd();
            }

            // Hook source might be a URL like "https://4syus.csb.app/src/App.js"
            // Parsed source map might be a partial path like "src/App.js"
            if (sourceMapIncludesSource(sourceMapJSON, runtimeSourceURL)) {
              hookSourceAndMetadata.sourceMapJSON = sourceMapJSON;

              // OPTIMIZATION If we've located a source map for this source,
              // we'll use it to retrieve the original source (to extract hook names).
              // We only fall back to parsing the full source code is when there's no source map.
              // The source is (potentially) very large,
              // So we
```

#### Final response (`no_budi`)

```text
**`React.createElement` → DOM insertion**

1. **Entrypoint**: `packages/react/src/jsx/ReactJSXElement.js:610` — `createElement(type, config, children)` returns a plain ReactElement object `{$$typeof, type, props, ...}`
2. **Scheduling**: `packages/react-dom/src/ReactDOMRoot.js` — `root.render(element)` calls `updateContainer`, enqueuing a `ReactDOMRoot` update via the reconciler
3. **Work loop**: `packages/react-reconciler/src/ReactFiberRootScheduler.js:608` — `performSyncWorkOnRoot(root, lanes)` drives the render phase
4. **Fiber traversal**: `packages/react-reconciler/src/ReactFiberWorkLoop.js:3054` — `performUnitOfWork(unitOfWork)` calls `beginWork` on each fiber
5. **Component execution**: `packages/react-reconciler/src/ReactFiberHooks.js:502` — `renderWithHooks(current, workInProgress, Component, props)` calls the function component, collecting hook state
6. **Child reconciliation**: `packages/react-reconciler/src/ReactFiberBeginWork.js:340` — `reconcileChildren(current, workInProgress, nextChildren)` diffs the new ReactElements against existing fibers
7. **Commit phase**: `packages/react-reconciler/src/ReactFiberWorkLoop.js:3698` — `commitRoot(root, ...)` walks the effect list and applies mutations
8. **DOM write**: `packages/react-dom-bindings/src/client/ReactFiberConfigDOM.js:1018` — `appendChild(parentInstance, child)` calls `parentInstance.appendChild(child)` on the real DOM node
```

#### Final response (`with_budi`)

```text
**`ReactDOM.createRoot(container).render(<App />)` flow:**

1. `createRoot()` — `packages/react-dom/src/client/ReactDOMRoot.js` → calls `createFiberRoot()`
2. `createFiberRoot()` — `packages/react-reconciler/src/ReactFiberRoot.js` → allocates the root Fiber tree
3. `ReactDOMRoot.prototype.render()` — `ReactDOMRoot.js` → calls `updateContainer()`
4. `updateContainer()` — `packages/react-reconciler/src/ReactFiberReconciler.js` → enqueues update, calls `scheduleUpdateOnFiber()`
5. `scheduleUpdateOnFiber()` — `packages/react-reconciler/src/ReactFiberWorkLoop.js:967` → triggers `performSyncWorkOnRoot()` or schedules concurrent work
6. `workLoopSync()` — `ReactFiberWorkLoop.js:2745` → loops calling `performUnitOfWork()` on each fiber
7. `beginWork()` — `packages/react-reconciler/src/ReactFiberBeginWork.js:4161` → reconciles each fiber node (diffs children)
8. `commitRoot()` — `packages/react-reconciler/src/ReactFiberCommitWork.js` → flushes mutations to the DOM
```

#### Judge rationale

```text
Both responses trace valid React render paths with file-level specificity. Response A (no_budi) is stronger because: (1) it includes line numbers at nearly every step, making each hop directly navigable; (2) it ends at the actual DOM write in `ReactFiberConfigDOM.js:appendChild`, completing the "entrypoint to output" requirement; (3) it includes the `renderWithHooks` step which is a meaningful intermediate showing how hook state is collected. Response B (with_budi) has a notable inaccuracy: it attributes `commitRoot()` to `ReactFiberCommitWork.js`, but `commitRoot` is defined in `ReactFiberWorkLoop.js` — `ReactFiberCommitWork.js` contains `commitMutationEffects` and related helpers. Response B also omits the final DOM write step, leaving the trace incomplete before actual output. Response B's starting from `createRoot` is a valid choice and slightly more idiomatic as an entrypoint, but the file misattribution and missing terminal step reduce its grounding and actionability scores.
```

</details>

### Flask

<details><summary>Flask · Prompt 1 · winner=with_budi</summary>

- Prompt: Give exactly 5 bullets describing repository architecture. Each bullet must include an exact file path. Keep total answer under 120 words. Do not edit files.
- Interactions: no_budi=3 / with_budi=2
- API duration ms: no_budi=10426 / with_budi=8627
- Cost USD: no_budi=0.06681 / with_budi=0.04269
- Hook output: success=True reason=ok context_chars=12000 retry=False

#### What budi injected

```text
[budi deterministic context]
rules:
- Use only file paths shown in snippets for exact-path answers.
- If snippets support fewer files than requested, return fewer instead of guessing.
snippets:
### src/flask/app.py:349-428 score=0.3869 reasons=lexical-hit,path-token-match,specific-path-hit,path-intent-boost,scope-miss,architecture-anchor-miss,clause-match
        # For one, it might be created while the server is running (e.g. during
        # development). Also, Google App Engine stores static files somewhere
        if self.has_static_folder:
            assert bool(static_host) == host_matching, (
                "Invalid static_host/host_matching combination"
            )
            # Use a weakref to avoid creating a reference cycle between the app
            # and the view function (see #3761).
            self_ref = weakref.ref(self)
            self.add_url_rule(
                f"{self.static_url_path}/<path:filename>",
                endpoint="static",
                host=static_host,
                view_func=lambda **kw: self_ref().send_static_file(**kw),  # type: ignore
            )

    def get_send_file_max_age(self, filename: str | None) -> int | None:
        """Used by :func:`send_file` to determine the ``max_age`` cache
        value for a given file path if it wasn't passed.

        By default, this returns :data:`SEND_FILE_MAX_AGE_DEFAULT` from
        the configuration of :data:`~flask.current_app`. This defaults
        to ``None``, which tells t
```

#### Final response (`no_budi`)

```text
- **Core app** — `src/flask/app.py` defines the `Flask` class (WSGI app, routing, request dispatching, lifecycle hooks).
- **Blueprints** — `src/flask/blueprints.py` provides modular route/handler grouping that registers onto the main app.
- **Context locals** — `src/flask/ctx.py` and `src/flask/globals.py` manage per-request/app context (`g`, `request`, `current_app`) via context vars.
- **Sessions & config** — `src/flask/sessions.py` handles signed cookie sessions; `src/flask/config.py` handles layered configuration loading.
- **CLI & testing** — `src/flask/cli.py` exposes the `flask` command via Click; `src/flask/testing.py` provides `FlaskClient` for test-time request simulation.
```

#### Final response (`with_budi`)

```text
- `src/flask/sansio/app.py` — Sans-I/O base app layer; contains framework-agnostic routing, config, and extension state, designed to be subclassed by WSGI/ASGI implementations.
- `src/flask/app.py` — Concrete `Flask` class; inherits from sansio, adds WSGI request/response handling, static file serving, and full request lifecycle.
- `src/flask/ctx.py` — Application and request context managers (`AppContext`, `RequestContext`) that push/pop `g`, `current_app`, and `request` globals.
- `src/flask/sansio/scaffold.py` — `Scaffold` base class shared by both `Flask` and `Blueprint`, providing route registration decorators (`@route`, `@get`, etc.).
- `src/flask/blueprints.py` — `Blueprint` implementation; defers route/handler registration until the blueprint is registered on an app.
```

#### Judge rationale

```text
Response B (with_budi) is clearly superior. It correctly identifies Flask's key architectural distinction: the sansio (Sans-I/O) layer at `src/flask/sansio/app.py` and `src/flask/sansio/scaffold.py`, which is central to modern Flask's design. It accurately describes `Scaffold` as the shared base for both `Flask` and `Blueprint`, and explains the inheritance chain from sansio → WSGI. Each bullet has exactly one file path as the prompt required. Response A (no_budi) misses the entire sansio layer entirely — a significant architectural omission — and combines multiple file paths per bullet (e.g., `ctx.py` and `globals.py` together, `sessions.py` and `config.py` together), violating the "exact file path" constraint and making the bullets less precise. Response B demonstrates deeper, repository-specific knowledge and strictly follows the prompt format.
```

</details>

<details><summary>Flask · Prompt 2 · winner=no_budi</summary>

- Prompt: Name up to 5 exact file paths where runtime configuration or environment variables are loaded/validated. Use only files you can verify. Keep concise. Do not edit files.
- Interactions: no_budi=3 / with_budi=1
- API duration ms: no_budi=11235 / with_budi=3372
- Cost USD: no_budi=0.03383 / with_budi=0.02766
- Hook output: success=True reason=ok context_chars=12000 retry=False

#### What budi injected

```text
[budi deterministic context]
rules:
- Use only file paths shown in snippets for exact-path answers.
- If snippets support fewer files than requested, return fewer instead of guessing.
- For runtime config/env prompts, prioritize runtime loaders/validators over build-time config.
snippets:
### src/flask/config.py:110-189 score=1.1761 reasons=lexical-hit,path-token-match,specific-path-hit,path-intent-boost,runtime-config-path-hit,runtime-config-code-hit,clause-match
        :param silent: set to ``True`` if you want silent failure for missing
                       files.
        :return: ``True`` if the file was loaded successfully.
        """
        rv = os.environ.get(variable_name)
        if not rv:
            if silent:
                return False
            raise RuntimeError(
                f"The environment variable {variable_name!r} is not set"
                " and as such configuration could not be loaded. Set"
                " this variable and make it point to a configuration"
                " file"
            )
        return self.from_pyfile(rv, silent=silent)

    def from_prefixed_env(
        self, prefix: str = "FLASK", *, loads: t.Callable[[str], t.Any] = json.loads
    ) -> bool:
        """Load any environment variables that start with ``FLASK_``,
        dropping the prefix from the env key for the config key. Values
        are passed through a loading function to attempt to convert them
        to more specific types than strings.

        Key
```

#### Final response (`no_budi`)

```text
1. `src/flask/config.py` — `Config.from_envvar()` and `from_prefixed_env()` load config from env vars
2. `src/flask/cli.py` — `load_dotenv()` function (line 698) reads `.env`/`.flaskenv` files and writes to `os.environ`
3. `src/flask/helpers.py` — `get_load_dotenv()` (line 36) and `get_debug_flag()` (line 32) read `FLASK_SKIP_DOTENV` / `FLASK_DEBUG`
4. `src/flask/app.py` — `Flask.run()` (line 697+) checks `FLASK_RUN_FROM_CLI` and triggers dotenv loading
5. `src/flask/logging.py` — reads env for debug-level logging configuration
```

#### Final response (`with_budi`)

```text
Based on the provided snippets:

1. `src/flask/config.py` — `Config` class with `from_envvar`, `from_prefixed_env`, `from_pyfile` methods
2. `tests/test_config.py` — tests for env var loading (`test_from_prefixed_env_nested`, `test_config_missing`)
3. `tests/test_instance_config.py` — tests for instance path/config loading
```

#### Judge rationale

```text
Response A (no_budi) correctly identifies 5 source files where runtime config/env vars are loaded, providing specific function names and line numbers (e.g., cli.py:698 for load_dotenv, helpers.py for FLASK_SKIP_DOTENV/FLASK_DEBUG). These are all plausible production code paths in a Flask repository. Response B (with_budi) only provides 3 entries and wastes 2 of them on test files (tests/test_config.py, tests/test_instance_config.py), which do not load or validate runtime configuration — they test it. The prompt explicitly asks for files where config is loaded/validated at runtime. Including test files is a meaningful error that reduces both correctness and actionability. Response A is clearly more complete, accurate, and useful.
```

</details>

<details><summary>Flask · Prompt 3 · winner=with_budi</summary>

- Prompt: Trace one concrete request/data flow from an entrypoint to output with exact file paths and function names in no more than 8 lines. Stop after one complete path. Do not edit files.
- Interactions: no_budi=3 / with_budi=1
- API duration ms: no_budi=11247 / with_budi=6853
- Cost USD: no_budi=0.04512 / with_budi=0.03239
- Hook output: success=True reason=ok context_chars=12004 retry=False

#### What budi injected

```text
[budi deterministic context]
rules:
- Use only file paths shown in snippets for exact-path answers.
- If snippets support fewer files than requested, return fewer instead of guessing.
snippets:
### examples/tutorial/tests/data.sql:1-8 score=0.1638 reasons=path-hit,path-token-match,specific-path-hit,path-intent-boost,scope-miss,test-fixture-penalty,clause-match
INSERT INTO user (username, password)
VALUES
  ('test', 'pbkdf2:sha256:50000$TCI4GzcX$0de171a4f4dac32e3364c7ddc7c14f3e2fa61f2d17574483f7ffbb431b4acb2f'),
  ('other', 'pbkdf2:sha256:50000$kJPKsz6N$d2d4784f1b030a9761f5ccaeeaca413f27f2ecb76d6168407af962ddce849f79');

INSERT INTO post (title, body, author_id, created)
VALUES
  ('test title', 'test' || x'0a' || 'body', 1, '2018-01-01 00:00:00');
### src/flask/cli.py:999-1045 score=0.1458 reasons=lexical-hit,weak-path-signal,scope-miss,clause-match
@click.command("shell", short_help="Run a shell in the app context.")
@with_appcontext
def shell_command() -> None:
    """Run an interactive Python shell in the context of a given
    Flask application.  The application will populate the default
    namespace of this shell according to its configuration.

    This is useful for executing small snippets of management code
    without having to manually configure the application.
    """
    import code

    banner = (
        f"Python {sys.version} on {sys.platform}\n"
        f"App: {current_app.import_name}\n"
        f"Instance: {current_app.instance_path}"
    )
    ctx: dict[s
```

#### Final response (`no_budi`)

```text
**HTTP GET `/` → response:**

1. **`app.py:1618` `Flask.__call__(environ, start_response)`** — WSGI server calls the app
2. → **`app.py:1566` `Flask.wsgi_app(environ, start_response)`** — pushes request/app contexts
3. → **`ctx.py` `RequestContext.__enter__`** — binds `request` and `session` globals
4. → **`app.py:992` `Flask.full_dispatch_request(ctx)`** — runs before-request hooks, then dispatches
5. → **`app.py:966` `Flask.dispatch_request(ctx)`** — calls `url_map.bind().match()` to find the view function
6. → **user view function** (e.g. `def index(): return "Hello"`) executes and returns a value
7. → **`app.py:1021` `Flask.finalize_request(rv)`** — calls `make_response(rv)` to build a `Response` object
8. → **`wsgi_app`** calls `response(environ, start_response)` and returns the iterable to the WSGI server
```

#### Final response (`with_budi`)

```text
1. `src/flask/app.py` — `Flask.wsgi_app()` receives the WSGI environ, creates `RequestContext`
2. `src/flask/ctx.py` — `RequestContext.push()` binds request/session to current context
3. `src/flask/app.py` — `Flask.full_dispatch_request()` calls `preprocess_request()` then `dispatch_request()`
4. `src/flask/app.py` — `Flask.dispatch_request()` matches URL via `self.url_map` and calls the matched view function
5. User view function executes, returns a string/Response
6. `src/flask/app.py` — `Flask.finalize_request()` passes return value to `make_response()`
7. `src/flask/wrappers.py` — `Response` object is built and returned up through `full_dispatch_request` → `wsgi_app`
8. WSGI server calls `response(environ, start_response)` to write headers and body to the client
```

#### Judge rationale

```text
Response B (with_budi) is better grounded and more actionable. It correctly uses `src/flask/app.py` and `src/flask/ctx.py` — the actual paths in the Flask source repo — while Response A omits the `src/flask/` prefix, making paths not directly navigable. Response A also invents specific line numbers (`:1618`, `:1566`, `:992`, `:966`, `:1021`) that are unverified and likely wrong for the current codebase, which is misleading rather than helpful. Response B correctly names `RequestContext.push()` as the binding mechanism (more accurate than A's `__enter__`) and adds `src/flask/wrappers.py` for Response construction, which is a genuine additional grounding detail. Both responses cover the same conceptual flow correctly and stay within 8 lines, but B's paths are accurate and navigable while A's line numbers create false precision.
```

</details>

### Express

<details><summary>Express · Prompt 1 · winner=with_budi</summary>

- Prompt: Give exactly 5 bullets describing repository architecture. Each bullet must include an exact file path. Keep total answer under 120 words. Do not edit files.
- Interactions: no_budi=5 / with_budi=3
- API duration ms: no_budi=17940 / with_budi=9188
- Cost USD: no_budi=0.08367 / with_budi=0.03564
- Hook output: success=True reason=ok context_chars=4792 retry=False

#### What budi injected

```text
[budi deterministic context]
rules:
- Use only file paths shown in snippets for exact-path answers.
- If snippets support fewer files than requested, return fewer instead of guessing.
snippets:
### test/app.router.js:3-3 score=0.4207 reasons=path-hit,path-token-match,specific-path-hit,path-intent-boost,scope-miss,architecture-anchor-hit
var after = require('after');
### examples/static-files/public/js/app.js:1-1 score=0.4082 reasons=semantic-hit,path-hit,path-token-match,specific-path-hit,path-intent-boost,scope-miss,architecture-anchor-miss,clause-match
// foo
### examples/static-files/index.js:9-9 score=0.3198 reasons=lexical-hit,path-token-match,specific-path-hit,path-intent-boost,scope-miss,architecture-anchor-miss,clause-match
var path = require('node:path');
### test/req.path.js:3-4 score=0.2995 reasons=path-hit,path-token-match,specific-path-hit,path-intent-boost,scope-miss,architecture-anchor-miss,clause-match
var express = require('../')
  , request = require('supertest');
### lib/application.js:24-24 score=0.2746 reasons=semantic-hit,path-token-match,specific-path-hit,path-intent-boost,scope-miss,architecture-anchor-miss,clause-match
var resolve = require('node:path').resolve;
### lib/application.js:22-22 score=0.2659 reasons=semantic-hit,path-token-match,specific-path-hit,path-intent-boost,scope-miss,architecture-anchor-miss
var compileQueryParser = require('./utils').compileQueryParser;
### test/app.response.js:3-3 score=0.1922 reasons=path-hit,path-token-match,sp
```

#### Final response (`no_budi`)

```text
- **Entry point** — `lib/express.js` exports the factory function that creates an app by merging `application`, `request`, and `response` prototypes, and re-exports the `Router`.
- **Application core** — `lib/application.js` defines the `app` prototype: middleware mounting (`use`), HTTP verb routing, settings engine, and the `listen`/`handle` lifecycle.
- **Router** — `lib/router/index.js` (not present here, but referenced) processes the middleware stack; route matching and `Layer` dispatch live alongside `lib/router/route.js`.
- **HTTP augmentation** — `lib/request.js` and `lib/response.js` extend Node's `IncomingMessage`/`ServerResponse` with helpers like `req.params`, `res.json`, and `res.render`.
- **View rendering** — `lib/view.js` resolves template file paths and delegates rendering to the configured engine (e.g., EJS, Handlebars), used by `res.render` in `lib/response.js`.
```

#### Final response (`with_budi`)

```text
- **`lib/express.js`** — Entry point; exports the `createApplication` factory and re-exports sub-modules (Router, Request, Response).
- **`lib/application.js`** — Core `app` object; handles settings, middleware mounting, template rendering, and HTTP server creation via the `router` package.
- **`lib/router/`** (external `router` package) — Routing layer consumed by `lib/application.js`; handles route matching and middleware dispatch.
- **`lib/request.js`** / **`lib/response.js`** — Extend Node's `http.IncomingMessage` and `ServerResponse` with Express-specific helpers.
- **`lib/view.js`** — Template engine abstraction; resolves view file paths and delegates rendering to registered engines.
```

#### Judge rationale

```text
Both responses cover the same five key files (lib/express.js, lib/application.js, lib/router/, lib/request.js+lib/response.js, lib/view.js) and are largely correct. Response B (with_budi) wins on three dimensions: (1) It is more concise and stays clearly within the 120-word limit. (2) Its characterization of lib/router/ as an external `router` package consumed by lib/application.js is the more accurate description for Express 5.x, where the router was extracted to a separate npm package. (3) It avoids the self-undermining aside that Response A includes — "not present here, but referenced" — which signals the model is guessing rather than reading the actual repo. Response A compensates slightly by naming specific helpers (req.params, res.json, res.render) and lib/router/route.js, adding concrete depth, but the hedging language hurts its grounding score. Overall Response B is cleaner, more confident, and better grounded in Express 5.x architecture.
```

</details>

<details><summary>Express · Prompt 2 · winner=no_budi</summary>

- Prompt: Name up to 5 exact file paths where runtime configuration or environment variables are loaded/validated. Use only files you can verify. Keep concise. Do not edit files.
- Interactions: no_budi=3 / with_budi=1
- API duration ms: no_budi=10178 / with_budi=4000
- Cost USD: no_budi=0.02890 / with_budi=0.02384
- Hook output: success=True reason=ok context_chars=5919 retry=False

#### What budi injected

```text
[budi deterministic context]
rules:
- Use only file paths shown in snippets for exact-path answers.
- If snippets support fewer files than requested, return fewer instead of guessing.
- For runtime config/env prompts, prioritize runtime loaders/validators over build-time config.
snippets:
### test/support/env.js:1-3 score=0.8512 reasons=semantic-hit,path-hit,path-token-match,specific-path-hit,path-intent-boost,runtime-config-path-hit,runtime-config-code-hit

process.env.NODE_ENV = 'test';
process.env.NO_DEPRECATION = 'body-parser,express';
### test/config.js:3-3 score=0.5700 reasons=path-hit,path-token-match,specific-path-hit,path-intent-boost,runtime-config-path-hit
var assert = require('node:assert');
### examples/static-files/index.js:8-8 score=0.3599 reasons=path-hit,path-token-match,specific-path-hit,path-intent-boost,runtime-config-miss,clause-match
var logger = require('morgan');
### examples/static-files/public/js/app.js:1-1 score=0.3593 reasons=path-hit,path-token-match,specific-path-hit,path-intent-boost,runtime-config-miss,clause-match
// foo
### test/res.sendFile.js:905-913 score=0.3185 reasons=lexical-hit,semantic-hit,path-token-match,specific-path-hit,path-intent-boost,runtime-config-miss,clause-match
function createApp(path, options, fn) {
  var app = express();

  app.use(function (req, res) {
    res.sendFile(path, options, fn);
  });

  return app;
}
### test/app.use.js:3-3 score=0.2999 reasons=path-hit,path-token-match,specific-path-hit,path-intent-boost,ru
```

#### Final response (`no_budi`)

```text
Here are the verified files where runtime configuration or environment variables are loaded/validated:

1. **`lib/application.js:91`** — reads `process.env.NODE_ENV` to set the app's default environment setting.
2. **`test/support/env.js`** — sets `process.env.NODE_ENV = 'test'` and `process.env.NO_DEPRECATION` for the test suite.
3. **`test/app.js:76–113`** — manipulates `process.env.NODE_ENV` in test setup/teardown hooks.
4. **`examples/error-pages/index.js:11`** — reads `process.env.NODE_ENV` to toggle silent mode.
5. **`examples/cookies/index.js:13`** — reads `process.env.NODE_ENV` to conditionally enable request logging.
```

#### Final response (`with_budi`)

```text
Based on the provided snippets:

1. `test/support/env.js` — sets `NODE_ENV` and `NO_DEPRECATION` env vars
2. `examples/route-map/index.js` — reads `process.env.NODE_ENV`
3. `lib/application.js` — reads `this.get('env')` (Express env config)
4. `examples/error/index.js` — reads env to control error logging
5. `examples/downloads/index.js` — uses `__dirname`-based path config
```

#### Judge rationale

```text
Response A (no_budi) is stronger on all dimensions. It provides specific line numbers (e.g., lib/application.js:91, test/app.js:76–113), making every entry immediately navigable. All 5 files are genuine instances of environment variable loading or validation. Response B (with_budi) hedges with "Based on the provided snippets" rather than verifying files directly, omits line numbers, and its 5th entry (examples/downloads/index.js using __dirname for path resolution) is not an example of env var loading/validation — it's a weak match that dilutes the answer's precision. Response B's lib/application.js entry also only references this.get('env') (internal Express config access) rather than the direct process.env.NODE_ENV read, which is less precise. Overall, Response A is more accurate, specific, and actionable.
```

</details>

<details><summary>Express · Prompt 3 · winner=no_budi</summary>

- Prompt: Trace one concrete request/data flow from an entrypoint to output with exact file paths and function names in no more than 8 lines. Stop after one complete path. Do not edit files.
- Interactions: no_budi=4 / with_budi=10
- API duration ms: no_budi=14791 / with_budi=27726
- Cost USD: no_budi=0.05311 / with_budi=0.11899
- Hook output: success=True reason=ok context_chars=7917 retry=False

#### What budi injected

```text
[budi deterministic context]
rules:
- Use only file paths shown in snippets for exact-path answers.
- If snippets support fewer files than requested, return fewer instead of guessing.
snippets:
### examples/static-files/public/js/app.js:1-1 score=0.4455 reasons=semantic-hit,path-hit,path-token-match,specific-path-hit,path-intent-boost,scope-miss,clause-match
// foo
### test/req.path.js:3-4 score=0.4282 reasons=path-hit,path-token-match,specific-path-hit,path-intent-boost,scope-miss,clause-match
var express = require('../')
  , request = require('supertest');
### examples/static-files/index.js:9-9 score=0.4062 reasons=path-hit,path-token-match,specific-path-hit,path-intent-boost,scope-miss,clause-match
var path = require('node:path');
### test/res.sendFile.js:3-3 score=0.3615 reasons=lexical-hit,path-token-match,specific-path-hit,path-intent-boost,scope-miss,clause-match
var after = require('after');
### test/res.sendFile.js:905-913 score=0.3220 reasons=semantic-hit,path-token-match,specific-path-hit,path-intent-boost,scope-miss,clause-match
function createApp(path, options, fn) {
  var app = express();

  app.use(function (req, res) {
    res.sendFile(path, options, fn);
  });

  return app;
}
### examples/downloads/index.js:13-13 score=0.0606 reasons=lexical-hit,semantic-hit,weak-path-signal,scope-miss,clause-match
var FILES_DIR = path.join(__dirname, 'files')
### lib/view.js:52-95 score=-0.0102 reasons=lexical-hit,semantic-hit,weak-path-signal,scope-miss,clause-match
functi
```

#### Final response (`no_budi`)

```text
**`GET /` → `res.send('Hello World')`**

1. `lib/application.js:210` — `app.listen()` creates an HTTP server with `app` as the request listener
2. Node.js HTTP server calls `app(req, res)` → `app.handle(req, res)` (`application.js:152`)
3. `application.js:177` — `this.router.handle(req, res, done)` delegates to the internal Router
4. `lib/router/index.js` — `proto.handle()` iterates the middleware stack, calling `next()`
5. `lib/router/index.js` — matching `Layer` is found; calls `layer.handle_request(req, res, next)`
6. `lib/router/layer.js` — `Layer.prototype.handle_request` invokes the route handler function
7. Route handler calls `res.send('Hello World')` → `lib/response.js:107` — `res.send()`
8. `response.js` sets headers and calls `res.end(body)` — Node.js writes the HTTP response
```

#### Final response (`with_budi`)

```text
**GET request → `res.send()` flow:**

1. `lib/application.js:598` `app.listen()` — creates `http.createServer(this)`, passing `app` as the request handler
2. Node.js HTTP server calls `app(req, res)` which invokes `lib/express.js` `createApplication` — the app function IS `app.handle`
3. `lib/application.js:152` `app.handle(req, res)` — sets `X-Powered-By` header, attaches `req.res`/`res.req`, sets prototypes from `this.request`/`this.response`
4. `lib/application.js:177` `this.router.handle(req, res, done)` — delegates to the `router` package's `Router#handle`
5. `router` package iterates layers, matching method + path, calls matched route's middleware stack
6. User middleware calls `res.send('hello')` → `lib/response.js` `res.send()` — sets `Content-Type`, `ETag`, `Content-Length`
7. `lib/response.js` `res.send()` calls `res.end(body)` — Node.js core writes the HTTP response to the socket
```

#### Judge rationale

```text
Response A (no_budi) provides a cleaner, more consistent 8-step trace where every step includes a specific file path or function name. The flow is logically correct: listen → app.handle → router.handle → Layer iteration → layer.handle_request → route handler → res.send → res.end. The mention of `Layer.prototype.handle_request` in `lib/router/layer.js` is accurate Express internals. Line 210 for `app.listen()` is likely off (it's closer to line 616 in Express v4), but the internal lines (152, 177) are plausible. Response B (with_budi) has three notable issues: (1) Step 2 incorrectly re-involves `lib/express.js` / `createApplication` after the HTTP server receives the request — the app callable goes directly to `app.handle`, not back through express.js; (2) Step 3 incorrectly states that `app.handle` sets the `X-Powered-By` header and attaches req/res prototypes — those are done during initialization/middleware, not in `handle()`; (3) Step 5 is vague ('router package iterates layers') with no file path or function name, breaking the trace's consistency. Response A's line 598 for `app.listen()` claimed by B is actually more realistic for Express v4, but the surrounding inaccuracies diminish its grounding value. Overall, A delivers a more reliable, fully-specific, end-to-end trace with consistent precision across all steps.
```

</details>

