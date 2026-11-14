/**
 * The diffui plugin API, version 0 (unstable through alpha).
 *
 * A plugin is a directory with a `plugin.toml` and an ES module, JavaScript or TypeScript:
 *
 *     ~/.config/diffui/plugins/<id>/plugin.toml
 *     ~/.config/diffui/plugins/<id>/main.js      (or main.ts)
 *
 * `plugin.toml` says what the plugin is, what it may do and what it adds.
 * Registration lives there, never in script:
 *
 *     id = "forgejo"
 *     name = "Forgejo"
 *     version = "0.1.0"
 *     api = 0
 *     main = "main.js"
 *
 *     [permissions]
 *     http = ["codeberg.org"]          # hosts http.fetch and auth.token may use; "*.example.com" and "localhost:3000" work
 *     exec = [["git", "credential"]]   # argv prefixes exec may run
 *     jj = "read"                      # "read": repo reads and repo.jj.query. "write": also repo.jj.run
 *
 *     [provides]
 *     forge = { hosts = ["codeberg.org"] }   # then main.js must call forge.register;
 *                                            # add term = "merge request" if that is what the forge calls them
 *
 *     [[commands]]                     # then main.js must call commands.register with this id
 *     id = "forgejo.open-in-browser"   # always "<plugin id>.<name>"
 *     label = "Open pull request in browser"
 *     when = "bookmark.pr"             # a when-clause; see `diffui skill`
 *     menu = ["bookmark", "palette"]   # palette | revision | bookmark | file
 *     keys = ["cmd+shift+o"]
 *
 *     [config.schema]                  # defaults for `config`; the user overrides them under [plugins.<id>] in config.toml
 *     host = { type = "string", default = "codeberg.org", label = "Host" }
 *
 * Hosts are named in two places that don't depend on each other. `[provides] forge.hosts` is what the plugin
 * claims: a repository whose remote is on one of them gets this forge. `[permissions] http` is what the script
 * may reach: `http.fetch` goes only there, and `auth.token(host)` answers only for a host listed there. A forge
 * host doesn't have to be in `http`, and a forge whose API lives on another host ("api.bitbucket.org" for
 * remotes on "bitbucket.org") lists the API's host in `http`. If it also wants the token git stores for the
 * remote, which is kept under the remote's host, it lists that host in `http` too and asks
 * `auth.token(repo.forgeRemote.host)`.
 *
 * The runtime is QuickJS (ES2023), not Node and not a browser:
 *
 * - Import from "diffui" and from relative files ("./util.js"). Nothing else resolves: no npm, no `require`,
 *   no `fs`, no `process`. There is no global `fetch`; use `http.fetch`.
 * - The only globals beyond the language are the ones declared at the bottom of this file. Type-check with
 *   `"lib": ["ES2023"]` (no "DOM") so that nothing else looks available. In JavaScript, put `// @ts-check` at
 *   the top of each file and set `"checkJs": true`.
 * - A `.ts` module is not compiled: diffui erases its types and runs the rest as written, so line numbers in
 *   errors are the file's. That is TypeScript's `"erasableSyntaxOnly": true` with `"verbatimModuleSyntax": true`:
 *   no `enum`, `namespace` or constructor parameter properties, and types are imported with `import type` or
 *   `import { type Repo }`. Import your own files by their real name ("./util.ts").
 * - Types are erased, not checked: a module with type errors loads and runs, and `diffui plugin check` doesn't
 *   see them. Check them yourself with the tsconfig.json (or jsconfig.json) `diffui plugin init` writes, from
 *   the plugin's folder: `npx -p typescript tsc --noEmit` (`-p jsconfig.json` for JavaScript). It finds the
 *   API's types because this file sits beside the module; without it there, `"diffui"` doesn't resolve.
 * - Plugins never draw. They answer questions (a forge provider), run commands and fill the slots in `ui`.
 * - Call `commands.register`, `forge.register` and `events.on` while the main module loads (top-level `await`
 *   is fine). Loading fails if a declared command or forge has no handler, or if anything throws.
 * - Every host function returns a promise. A refusal rejects with an `Error` whose `name` is "PermissionError";
 *   a timeout with "TimeoutError"; a cancelled call with "AbortError".
 * - A handler that runs for more than half a second without awaiting, or a heap past 64 MB, stops the plugin
 *   until it is reloaded.
 */
declare module "diffui" {
  // Registration

  /** Handlers for the commands `plugin.toml` declares under `[[commands]]`. */
  export const commands: Commands;

  export interface Commands {
    /**
     * Set the handler for a declared command. Registering an id the manifest doesn't declare throws, and so
     * does registering twice. The command is "running" until the returned promise settles; a rejection is
     * shown to the user as the command's error.
     *
     *     commands.register("forgejo.open-in-browser", async ({ repo, bookmark }) => {
     *       await ui.openUrl(`${repo.forgeRemote.webUrl}/pulls?head=${bookmark}`);
     *     });
     */
    register(id: string, handler: (context: CommandContext) => void | Promise<void>): void;
  }

  /** Where a command was invoked. Fields that don't apply are `null`. */
  export interface CommandContext {
    readonly repo: Repo;
    /**
     * The revision in context: the row a menu was opened on, the revision of the bookmark in context, or the
     * one a palette result was picked for. Run from the palette's top level or by a key, it is the selected
     * revision (one row: the marked rows are in `repo.selection()`). `null` when there is none, or when it
     * isn't in the log on screen.
     */
    readonly changeId: string | null;
    readonly commitId: string | null;
    /** The bookmark in context, for a command placed in the bookmark menu. */
    readonly bookmark: string | null;
    /** The file in context, relative to `repo.root`. */
    readonly path: string | null;
    /** A 1-based line in `path`: line-select mode's cursor when it is in that file, else its first changed line. */
    readonly line: number | null;
  }

  /**
   * The forge provider, for a plugin whose manifest has `[provides] forge`. diffui matches a repository's
   * remotes against the manifest's hosts, asks the provider for data and draws the badges, dialogs and
   * stacked pull requests itself.
   */
  export const forge: Forge;

  export interface Forge {
    /** Register the provider. Call it once, while the main module loads. */
    register(provider: ForgeProvider): void;
  }

  /**
   * What diffui asks a forge. Only `pullRequests` is required; a method left out turns the matching feature
   * off for this forge. Every method gets the repository with `repo.forgeRemote` set. Throw to report a
   * failure: the error's message is shown to the user. Each call must settle within a minute.
   */
  export interface ForgeProvider {
    /**
     * The pull requests whose head branch is one of `branches`, in any state. Leave out the ones from forks,
     * whose branch can have the same name: keep a pull request only when its source repository is the one
     * `repo.forgeRemote` names (compare the owner and name, or the repository's id against the target's).
     */
    pullRequests(repo: Repo, query: { branches: string[] }): Promise<PullRequest[]>;
    /**
     * The combined CI state of a commit. `sha` is the `headSha` of a pull request `pullRequests` returned, so
     * return full commit ids there. Asked for open and draft pull requests only.
     */
    checks?(repo: Repo, sha: string): Promise<Checks>;
    /** Open a pull request and return it. The head branch is already pushed. */
    createPullRequest?(repo: Repo, request: CreatePullRequest): Promise<PullRequest>;
    /** Change the fields present in `patch` and return the pull request. */
    updatePullRequest?(repo: Repo, number: number, patch: UpdatePullRequest): Promise<PullRequest>;
  }

  export interface PullRequest {
    number: number;
    /** The page to open in a browser. */
    url: string;
    title: string;
    /** Head branch name. */
    head: string;
    /** Base branch name. */
    base: string;
    /** The head commit's full id. */
    headSha: string;
    state: "open" | "draft" | "closed" | "merged";
    review?: "approved" | "changesRequested" | "reviewRequired" | null;
  }

  export interface Checks {
    /** "none" when the commit has no checks at all. */
    state: "pending" | "success" | "failure" | "none";
    /** Where to read about them. */
    url?: string | null;
  }

  export interface CreatePullRequest {
    head: string;
    base: string;
    title: string;
    body: string;
    draft: boolean;
  }

  /** The fields to change. One that isn't changing is absent: a field is a string or missing, never `null`. */
  export interface UpdatePullRequest {
    title?: string;
    body?: string;
    base?: string;
  }

  /** Things that happened in the app. Fire and forget: what a listener returns is ignored, what it throws is logged. */
  export const events: Events;

  export interface Events {
    /** Listen for an event. Call it while the main module loads. */
    on<Name extends keyof EventMap>(name: Name, listener: (event: EventMap[Name]) => void | Promise<void>): void;
  }

  /** Each event's payload. All carry the repository it happened in. */
  export interface EventMap {
    /** A repository tab finished opening. Also sent, once the plugin has loaded, for each one already open. */
    repoOpened: { readonly repo: Repo };
    /** Bookmarks or remote refs moved. */
    refsChanged: { readonly repo: Repo };
    /**
     * The repository changed: a commit, a rebase, an undo, a fetch that brought something, an edit to the
     * working copy once it has been snapshotted. Sent when the log on screen is at a jj operation other
     * than the last one, so `repo` reads the new state. One event however many operations went by; never
     * for a git repository. The event to recompute a status item or a cache on. When bookmarks moved too,
     * `refsChanged` is sent as well.
     */
    operationChanged: { readonly repo: Repo };
    /** The selected revisions or the selected file changed, and stayed put for a moment. Read them with `repo.selection()`. */
    selectionChanged: { readonly repo: Repo };
    /** A fetch finished, and the log shows what it brought. */
    fetched: { readonly repo: Repo };
    /** A push finished; `bookmarks` are the ones it sent. */
    pushed: { readonly repo: Repo; readonly bookmarks: string[] };
    /** A command ran (any command, not only this plugin's). */
    commandInvoked: { readonly repo: Repo; readonly id: string };
  }

  // The repository handle

  /**
   * One repository as it was when the handle was made. diffui passes one to every handler; there is no
   * "current repository". The handle is pinned to the tab and the operation that were on screen: after the
   * repository moves on (a commit, a fetch, an undo), or while another tab is in front, its methods reject
   * with an `Error` named "StaleError", so a slow plugin can't act on something the user no longer sees.
   * Don't keep handles; use the one you are given. (`root` and `forgeRemote` are plain values and stay
   * readable.)
   *
   * `root` and `forgeRemote` need no permission. Every method needs `jj = "read"` in the manifest, and
   * `jj.run` needs `jj = "write"`.
   */
  export interface Repo {
    /** The workspace root, absolute. */
    readonly root: string;
    /** The remote that belongs to this plugin's forge, already parsed. `null` when there is none. */
    readonly forgeRemote: ForgeRemote | null;
    /** Run jj command lines. */
    readonly jj: Jj;
    /** Every bookmark, local and remote. */
    bookmarks(): Promise<Bookmark[]>;
    /** The git remotes. */
    remotes(): Promise<Remote[]>;
    /** What the user has selected. */
    selection(): Promise<Selection>;
    /**
     * The revisions a revset matches, newest first (the order of `jj log`). `limit` defaults to 100; when
     * the revset matches more, the newest are kept. Rejects with jj's message when the revset doesn't parse
     * or names a revision that doesn't exist.
     */
    rows(revset: string, options?: { limit?: number }): Promise<Row[]>;
    /**
     * The diff of the revisions a revset matches, as `jj diff -r` makes it. Several revisions give one
     * combined diff, from before the oldest to the newest (`a::c` is the change from `a`'s parent to `c`),
     * not a diff per revision; a set with a gap in it (`a | c`) rejects with jj's message, as a revset that
     * doesn't resolve does. `format` defaults to "git", `maxBytes` to 1 MB.
     */
    diff(revset: string, options?: { format?: DiffFormat; maxBytes?: number }): Promise<Diff>;
    /** A file's text at a revision; `null` when the revision has no such file. `path` is relative to `root`. */
    readFile(rev: string, path: string): Promise<string | null>;
  }

  export interface Jj {
    /**
     * Run a read-only jj command and resolve with its stdout. It runs at the handle's operation and never
     * snapshots the working copy. Arguments are an argv, not a shell line:
     *
     *     const ids = await repo.jj.query(["log", "--no-graph", "-r", "trunk()..@", "-T", 'change_id ++ "\n"']);
     *
     * The argv must start with one of: `log`, `show`, `diff`, `interdiff`, `evolog`, `status`, `file show`,
     * `file list`, `file annotate`, `bookmark list`, `tag list`, `op log`, `op show`, `config get`,
     * `git remote list`, `workspace list`. It may not carry `-R`, `--at-op`, `--ignore-working-copy`
     * (diffui sets them), `--tool` or `--config*`. Rejects with why when the line is refused, and with
     * jj's stderr when the command fails.
     */
    query(args: string[]): Promise<string>;
    /**
     * Run a jj command that changes the repository, exactly as if the user had typed it in the command
     * palette: it shows in the activity log, asks before rewriting immutable revisions and can be undone.
     * Needs `jj = "write"`.
     *
     *     await repo.jj.run(["bookmark", "set", "wip", "-r", "@"]);
     *
     * It resolves once diffui has accepted the command, not when it has finished, and rejects with why
     * when the line is refused. Once the command has changed the repository this handle is stale: do the
     * work in one command (`jj new -m …`, not `new` then `describe`), or carry on from the next event.
     */
    run(args: string[]): Promise<JjRun>;
  }

  export interface JjRun {
    /** What the command did, in a sentence. */
    summary: string;
  }

  export interface ForgeRemote {
    /** As in the remote's URL, with the port when it has one ("codeberg.org", "localhost:3000"). */
    host: string;
    /** Owner or group path ("team", "group/subgroup"). */
    owner: string;
    name: string;
    /** The project's page, scheme included ("https://codeberg.org/team/app"). Build API URLs from its origin. */
    webUrl: string;
  }

  export interface Bookmark {
    name: string;
    /** Commit ids the local bookmark points at: one normally, several when conflicted, none when remote-only. */
    targets: string[];
    remotes: RemoteBookmark[];
  }

  export interface RemoteBookmark {
    remote: string;
    target: string;
    tracked: boolean;
  }

  export interface Remote {
    name: string;
    url: string | null;
  }

  export interface Selection {
    /** The selected revisions, the cursor's first. */
    revisions: RevisionRef[];
    /** The file open in the diff, relative to the root. */
    path: string | null;
  }

  export interface RevisionRef {
    changeId: string;
    commitId: string;
  }

  export interface Row {
    changeId: string;
    commitId: string;
    /**
     * The first line of the description; "" when there is none. For all of it:
     * `repo.jj.query(["log", "--no-graph", "-r", commitId, "-T", "description"])`.
     */
    description: string;
    author: string;
    /** Author time in milliseconds since the epoch. */
    timestamp: number | null;
    /** Local bookmarks on this revision. */
    bookmarks: string[];
    workingCopy: boolean;
    immutable: boolean;
    empty: boolean;
    conflict: boolean;
  }

  export type DiffFormat = "git" | "stat" | "summary" | "name-only";

  export interface Diff {
    text: string;
    /** The diff was longer than `maxBytes` and was cut. */
    truncated: boolean;
  }

  // Host functions

  /** HTTP, to the hosts listed under `[permissions] http`. */
  export const http: Http;

  export interface Http {
    /**
     * Like `fetch`, with text bodies. A 4xx or 5xx resolves (check `ok`); the promise rejects when the host
     * isn't allowed ("PermissionError"), on a network failure ("NetworkError") or a timeout ("TimeoutError").
     * Redirects are followed only to allowed hosts. Responses larger than 8 MB are refused.
     *
     *     const res = await http.fetch(`${origin}/api/v1/user`, { headers: { authorization: `token ${token}` } });
     *     if (!res.ok) throw new Error(`Forgejo ${res.status}: ${await res.text()}`);
     *     const user = await res.json();
     */
    fetch(url: string | URL, init?: RequestInit): Promise<Response>;
  }

  export interface RequestInit {
    /** Defaults to "GET". */
    method?: "GET" | "HEAD" | "POST" | "PUT" | "PATCH" | "DELETE" | "OPTIONS";
    headers?: Record<string, string> | Iterable<[string, string]>;
    /** Text only; `JSON.stringify` objects yourself. A `URLSearchParams` is sent as a form. */
    body?: string | URLSearchParams | null;
    signal?: AbortSignal;
    /** Give up after this long. Defaults to 60 000. */
    timeoutMs?: number;
  }

  export interface Response {
    /** `status` is 200 to 299. */
    readonly ok: boolean;
    readonly status: number;
    readonly statusText: string;
    /** The URL that answered, after redirects. */
    readonly url: string;
    readonly headers: Headers;
    /** The body as text. May be called more than once. */
    text(): Promise<string>;
    /** The body parsed as JSON. */
    json(): Promise<any>;
  }

  export interface Headers {
    /** The header's value, case-insensitively; repeated headers are joined with ", ". */
    get(name: string): string | null;
    has(name: string): boolean;
    entries(): IterableIterator<[string, string]>;
    [Symbol.iterator](): IterableIterator<[string, string]>;
  }

  /**
   * Run a program. `argv` must start with one of the prefixes under `[permissions] exec`
   * (`[["git", "credential"]]` allows `["git", "credential", "fill"]`, not `["git", "push"]`). There is no
   * shell: `argv[0]` is looked up on PATH and the rest are passed as they are. A non-zero exit resolves
   * (check `ok`); the promise rejects when the argv isn't allowed, the program can't start, it runs past
   * its timeout ("TimeoutError") or `signal` aborts ("AbortError"), which also kills it.
   *
   *     const out = await exec(["git", "config", "--get", "user.email"], { cwd: repo.root });
   *     if (out.ok) console.log(out.stdout.trim());
   */
  export function exec(argv: string[], options?: ExecOptions): Promise<ExecResult>;

  export interface ExecOptions {
    /** Text to feed its stdin. Without it stdin is closed. */
    stdin?: string;
    /** Defaults to the plugin's own directory. Pass `repo.root` to run in the repository. */
    cwd?: string;
    signal?: AbortSignal;
    /** Called with each line of stdout as it arrives, without its newline. */
    onStdout?: (line: string) => void;
    /** Give up after this long. Defaults to 300 000. */
    timeoutMs?: number;
  }

  export interface ExecResult {
    /** `code` is 0. */
    readonly ok: boolean;
    /** The exit code; `null` when a signal ended it. */
    readonly code: number | null;
    readonly stdout: string;
    readonly stderr: string;
  }

  /** Credentials. diffui stores none. */
  export const auth: Auth;

  export interface Auth {
    /**
     * A token for `host`, which must be listed under `[permissions] http`. It comes from the user's
     * `token_cmd` for this plugin when set (`[plugins.<id>] token_cmd = ["pass", "show", "{host}"]` in
     * config.toml), otherwise from git's credential helpers (`git credential fill`). It never prompts:
     * with nothing stored it rejects with a message telling the user how to store one. Cached for five
     * minutes.
     */
    token(host: string): Promise<string>;
  }

  /** A small JSON store that belongs to this plugin and survives restarts. At most 1 MB in total. */
  export const kv: Kv;

  export interface Kv {
    /** The stored value, or `undefined`. */
    get(key: string): Promise<any>;
    /** Store anything `JSON.stringify` can write. */
    set(key: string, value: unknown): Promise<void>;
    delete(key: string): Promise<void>;
  }

  /**
   * The plugin's settings: the defaults from `[config.schema]` in `plugin.toml`, under whatever the user
   * wrote in `[plugins.<id>]` in config.toml. Frozen; read once when the plugin loads.
   */
  export const config: Readonly<Record<string, any>>;

  /** The places a plugin can show something. */
  export const ui: Ui;

  export interface Ui {
    /** Type a value, confirm, or pick from a list. One prompt at a time. */
    readonly prompt: Prompt;
    readonly clipboard: Clipboard;
    /** This plugin's item in the status bar. */
    readonly status: Status;
    /** A short message that goes away by itself. Sentence case, no trailing period. */
    toast(message: string, options?: { tone?: "info" | "error" }): Promise<void>;
    /**
     * A row in the activity log (of the tab in front when it starts) for work that takes a while. Finish
     * every activity you start.
     *
     *     const activity = await ui.activity("Syncing labels");
     *     try { await sync(activity); await activity.finish(); }
     *     catch (error) { await activity.finish({ error: error.message }); }
     */
    activity(label: string): Promise<Activity>;
    /** Open an http(s) link in the user's browser. */
    openUrl(url: string | URL): Promise<void>;
  }

  export interface Activity {
    /** Add a line to the row's output. */
    append(text: string): Promise<void>;
    /** Mark it done, or failed when `error` is given. */
    finish(result?: { error?: string }): Promise<void>;
  }

  export interface Prompt {
    /** Ask for a line of text. Resolves with it, or `null` when dismissed. */
    input(options: { label: string; value?: string; placeholder?: string }): Promise<string | null>;
    /** Ask yes or no. `destructive` styles the confirm button as dangerous. */
    confirm(options: { title: string; body?: string; confirmLabel?: string; destructive?: boolean }): Promise<boolean>;
    /**
     * Pick one, in the palette. Resolves with the choice's `value` (its label for a plain string), or `null`
     * when dismissed. Typing filters the choices by label (a fuzzy match; hints and values aren't searched),
     * and at most 200 matches are listed at once, so a long list is reached by typing. Give each choice its
     * own label: the pick is found again by it.
     */
    choose(options: { label: string; choices: Array<string | Choice> }): Promise<string | null>;
  }

  export interface Choice {
    label: string;
    /** Dimmed text after the label. */
    hint?: string;
    /** Defaults to the label. */
    value?: string;
  }

  export interface Clipboard {
    write(text: string): Promise<void>;
  }

  export interface Status {
    /**
     * Show `text` in the status bar; `null` (or blank text) removes the item. A plugin has one item, and it
     * belongs to the window, not to a repository: it stays as it is when the user switches tabs. Only its
     * first line shows, cut with an ellipsis past 220 px.
     */
    set(text: string | null): Promise<void>;
  }
}

// Globals. These, the ECMAScript built-ins and nothing else.

/**
 * Logging. Lines go to diffui's log file and to the plugin's log in the app, with the level as given.
 * Objects are printed as JSON, errors with their stack.
 */
declare const console: {
  debug(...values: unknown[]): void;
  log(...values: unknown[]): void;
  info(...values: unknown[]): void;
  warn(...values: unknown[]): void;
  error(...values: unknown[]): void;
};

/** Call `callback` once, after `delay` milliseconds (0 by default). Returns an id for `clearTimeout`. */
declare function setTimeout<Args extends unknown[]>(
  callback: (...args: Args) => void,
  delay?: number,
  ...args: Args
): number;
declare function clearTimeout(id: number | undefined): void;

declare class URL {
  /** Throws a `TypeError` when `url` (resolved against `base`, if given) isn't a URL. */
  constructor(url: string | URL, base?: string | URL);
  static canParse(url: string | URL, base?: string | URL): boolean;
  readonly href: string;
  /** "https://host:port", without the port when it is the scheme's default. */
  readonly origin: string;
  /** With its colon: "https:". */
  readonly protocol: string;
  readonly username: string;
  readonly password: string;
  /** Hostname and, when not the default, port. */
  readonly host: string;
  readonly hostname: string;
  readonly port: string;
  pathname: string;
  /** With its question mark, or "". */
  search: string;
  /** Live: changing it changes `search`. */
  readonly searchParams: URLSearchParams;
  /** With its number sign, or "". */
  hash: string;
  toString(): string;
  toJSON(): string;
}

declare class URLSearchParams {
  constructor(init?: string | Record<string, string> | Iterable<[string, string]>);
  readonly size: number;
  append(name: string, value: string): void;
  delete(name: string): void;
  get(name: string): string | null;
  getAll(name: string): string[];
  has(name: string): boolean;
  set(name: string, value: string): void;
  sort(): void;
  forEach(callback: (value: string, name: string, params: URLSearchParams) => void): void;
  entries(): IterableIterator<[string, string]>;
  keys(): IterableIterator<string>;
  values(): IterableIterator<string>;
  [Symbol.iterator](): IterableIterator<[string, string]>;
  toString(): string;
}

/** UTF-8 only. */
declare class TextEncoder {
  readonly encoding: "utf-8";
  encode(text?: string): Uint8Array;
}

/** UTF-8 only. Invalid bytes decode to U+FFFD unless `fatal` is set, which throws a `TypeError` instead. */
declare class TextDecoder {
  constructor(label?: "utf-8" | "utf8", options?: { fatal?: boolean });
  readonly encoding: "utf-8";
  readonly fatal: boolean;
  decode(input?: ArrayBuffer | ArrayBufferView): string;
}

/** Cancels `http.fetch` and `exec` calls that were given its `signal`. */
declare class AbortController {
  readonly signal: AbortSignal;
  /** Rejects the calls holding the signal with `reason`, or with an "AbortError". */
  abort(reason?: unknown): void;
}

declare class AbortSignal {
  /** A signal that is already aborted. */
  static abort(reason?: unknown): AbortSignal;
  /** A signal that aborts by itself after `ms` milliseconds, with a "TimeoutError". */
  static timeout(ms: number): AbortSignal;
  readonly aborted: boolean;
  readonly reason: unknown;
  onabort: ((event: { type: "abort"; target: AbortSignal }) => void) | null;
  throwIfAborted(): void;
  addEventListener(type: "abort", listener: (event: { type: "abort"; target: AbortSignal }) => void): void;
  removeEventListener(type: "abort", listener: (event: { type: "abort"; target: AbortSignal }) => void): void;
}
