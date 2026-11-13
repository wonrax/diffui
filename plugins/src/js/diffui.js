// The "diffui" module: everything a plugin imports, and the web-style globals.
//
// The host evaluates this once per context, before the plugin's main module.
// It talks to Rust through three native functions on `__diffui_boot.native`
// and hands Rust two entry points back (`dispatch`, `finishLoad`). Every host
// call is a JSON message with an id; the reply settles the promise by id.
// `assets/diffui.d.ts` is the contract; a test keeps the two in step.

const boot = globalThis.__diffui_boot;
delete globalThis.__diffui_boot;
const { send, utf8Encode, utf8Decode } = boot.native;
const declaredCommands = new Set(boot.commands);
const providesForge = boot.forge;
const eventNames = boot.events;

const post = (message) => send(JSON.stringify(message));

function freezeDeep(value) {
  if (value && typeof value === "object" && !Object.isFrozen(value)) {
    Object.freeze(value);
    for (const key of Reflect.ownKeys(value)) freezeDeep(value[key]);
  }
  return value;
}

// Host calls.

let nextCall = 1;
const pendingCalls = new Map();

function abortError(signal) {
  if (signal.reason !== undefined) return signal.reason;
  const error = new Error("The operation was aborted");
  error.name = "AbortError";
  return error;
}

/** Ask the host; resolves with its reply. `signal` cancels, `onChunk` hears streamed output. */
function call(op, args, { signal, onChunk } = {}) {
  // Made here so a failure points at the plugin's line, not at the reply.
  const failure = new Error();
  return new Promise((resolve, reject) => {
    if (signal?.aborted) return reject(abortError(signal));
    const id = nextCall++;
    const onAbort = () => {
      post({ t: "cancel", id });
      settle(reject, abortError(signal));
    };
    const settle = (finish, value) => {
      pendingCalls.delete(id);
      signal?.removeEventListener("abort", onAbort);
      finish(value);
    };
    pendingCalls.set(id, {
      onChunk,
      resolve: (value) => settle(resolve, value),
      reject: ({ name, message }) => {
        failure.name = name;
        failure.message = message;
        settle(reject, failure);
      },
    });
    signal?.addEventListener("abort", onAbort);
    post({ t: "call", id, op, args });
  });
}

// Globals: timers.

let nextTimer = 1;
const timers = new Map();

globalThis.setTimeout = (callback, delay = 0, ...args) => {
  if (typeof callback !== "function") throw new TypeError("setTimeout needs a function");
  const id = nextTimer++;
  timers.set(id, () => callback(...args));
  post({ t: "timer", id, ms: Math.max(0, Number(delay) || 0) });
  return id;
};
globalThis.clearTimeout = (id) => {
  timers.delete(id);
};

// Globals: AbortController.

let abort;

class AbortSignal {
  #aborted = false;
  #reason = undefined;
  #listeners = [];
  onabort = null;

  static {
    abort = (signal, reason) => {
      if (signal.#aborted) return;
      signal.#aborted = true;
      signal.#reason = reason;
      const event = { type: "abort", target: signal };
      const listeners = signal.#listeners;
      signal.#listeners = [];
      if (typeof signal.onabort === "function") signal.onabort(event);
      for (const listener of listeners) listener(event);
    };
  }

  get aborted() {
    return this.#aborted;
  }
  get reason() {
    return this.#reason;
  }
  throwIfAborted() {
    if (this.#aborted) throw abortError(this);
  }
  addEventListener(type, listener) {
    if (type === "abort" && !this.#aborted && !this.#listeners.includes(listener)) {
      this.#listeners.push(listener);
    }
  }
  removeEventListener(type, listener) {
    if (type === "abort") this.#listeners = this.#listeners.filter((l) => l !== listener);
  }
  static abort(reason) {
    const signal = new AbortSignal();
    abort(signal, reason);
    return signal;
  }
  static timeout(ms) {
    const signal = new AbortSignal();
    setTimeout(() => {
      const error = new Error("The operation timed out");
      error.name = "TimeoutError";
      abort(signal, error);
    }, ms);
    return signal;
  }
}

class AbortController {
  signal = new AbortSignal();
  abort(reason) {
    abort(this.signal, reason);
  }
}

globalThis.AbortSignal = AbortSignal;
globalThis.AbortController = AbortController;

// Globals: text encoding.

globalThis.TextEncoder = class TextEncoder {
  get encoding() {
    return "utf-8";
  }
  encode(text = "") {
    return utf8Encode(String(text));
  }
};

globalThis.TextDecoder = class TextDecoder {
  #fatal;
  constructor(label = "utf-8", options = {}) {
    if (!/^(utf-?8|unicode-1-1-utf-8)$/i.test(String(label).trim())) {
      throw new RangeError(`TextDecoder only decodes utf-8, not "${label}"`);
    }
    this.#fatal = Boolean(options.fatal);
  }
  get encoding() {
    return "utf-8";
  }
  get fatal() {
    return this.#fatal;
  }
  decode(input = new Uint8Array()) {
    const bytes =
      input instanceof Uint8Array
        ? input
        : ArrayBuffer.isView(input)
          ? new Uint8Array(input.buffer, input.byteOffset, input.byteLength)
          : new Uint8Array(input);
    return utf8Decode(bytes, this.#fatal);
  }
};

// Globals: URL.

const SPECIAL_PORTS = { "http:": "80", "https:": "443", "ws:": "80", "wss:": "443", "ftp:": "21" };
const URL_PARTS =
  /^([A-Za-z][A-Za-z0-9+.-]*:)?(?:\/\/(?:([^\/?#@]*)@)?(\[[^\]]*\]|[^\/?#:]*)(?::(\d*))?)?([^?#]*)(\?[^#]*)?(#.*)?$/s;

/** Percent-encode what `unsafe` matches, and any `%` that isn't already an escape. */
const escapeWith = (unsafe) => (text) =>
  text.replace(unsafe, (char) => encodeURIComponent(char)).replace(/%(?![0-9A-Fa-f]{2})/g, "%25");
const escapePath = escapeWith(/[^A-Za-z0-9\-._~!$&'()*+,;=:@\/%]/gu);
const escapeQuery = escapeWith(/[^A-Za-z0-9\-._~!$&'()*+,;=:@\/?%]/gu);
const escapeHash = escapeWith(/[^A-Za-z0-9\-._~!$&'()*+,;=:@\/?#%]/gu);
const escapeUser = escapeWith(/[^A-Za-z0-9\-._~!$&'()*+,;=%]/gu);

function removeDots(path) {
  const out = [];
  const parts = path.split("/");
  parts.forEach((part, index) => {
    const last = index === parts.length - 1;
    if (part === "..") {
      if (out.length > 1) out.pop();
      if (last) out.push("");
    } else if (part === ".") {
      if (last) out.push("");
    } else {
      out.push(part);
    }
  });
  return out.join("/");
}

function parseUrl(input, base) {
  const text = String(input).trim();
  const match = URL_PARTS.exec(text);
  let [, protocol, userinfo, host, port, path, search, hash] = match;
  const hasAuthority = /^([A-Za-z][A-Za-z0-9+.-]*:)?\/\//.test(text);
  if (!protocol) {
    if (!base) throw new TypeError(`Invalid URL: "${text}"`);
    protocol = base.protocol;
    if (!hasAuthority) {
      userinfo = base.userinfo;
      host = base.hostname;
      port = base.port;
      if (path === "") {
        path = base.pathname;
        search ??= base.search;
      } else if (!path.startsWith("/")) {
        path = base.pathname.replace(/[^\/]*$/, "") + path;
      }
    }
  }
  protocol = protocol.toLowerCase();
  const special = protocol in SPECIAL_PORTS;
  if (special && !host) throw new TypeError(`Invalid URL: "${text}"`);
  if (port && Number(port) > 65535) throw new TypeError(`Invalid URL: "${text}"`);
  const [username = "", password = ""] = (userinfo ?? "").split(/:(.*)/s);
  if (hasAuthority || !match[1]) path = removeDots(path);
  if (special && path === "") path = "/";
  return {
    protocol,
    userinfo: userinfo ?? "",
    username: escapeUser(username),
    password: escapeUser(password),
    hostname: (host ?? "").toLowerCase(),
    port: port && port !== SPECIAL_PORTS[protocol] ? String(Number(port)) : "",
    pathname: hasAuthority || base ? escapePath(path) : path,
    search: search && search !== "?" ? escapeQuery(search) : "",
    hash: hash && hash !== "#" ? escapeHash(hash) : "",
    hasAuthority: hasAuthority || (!match[1] && Boolean(base)),
  };
}

const decodeForm = (text) => {
  const spaced = text.replace(/\+/g, " ");
  try {
    return decodeURIComponent(spaced);
  } catch {
    return spaced;
  }
};
const encodeForm = (text) =>
  encodeURIComponent(text).replace(/%20/g, "+").replace(/[!'()~]/g, (c) => "%" + c.charCodeAt(0).toString(16).toUpperCase());

class URLSearchParams {
  #pairs = [];
  #changed = null;

  constructor(init = "", changed = null) {
    this.#changed = changed;
    if (typeof init === "string") {
      for (const pair of init.replace(/^\?/, "").split("&")) {
        if (pair === "") continue;
        const [name, value = ""] = pair.split(/=(.*)/s);
        this.#pairs.push([decodeForm(name), decodeForm(value)]);
      }
    } else if (init && typeof init[Symbol.iterator] === "function") {
      for (const [name, value] of init) this.#pairs.push([String(name), String(value)]);
    } else if (init && typeof init === "object") {
      for (const name of Object.keys(init)) this.#pairs.push([name, String(init[name])]);
    }
  }
  #touch() {
    this.#changed?.(this.toString());
  }
  get size() {
    return this.#pairs.length;
  }
  append(name, value) {
    this.#pairs.push([String(name), String(value)]);
    this.#touch();
  }
  delete(name) {
    this.#pairs = this.#pairs.filter(([n]) => n !== String(name));
    this.#touch();
  }
  get(name) {
    return this.#pairs.find(([n]) => n === String(name))?.[1] ?? null;
  }
  getAll(name) {
    return this.#pairs.filter(([n]) => n === String(name)).map(([, v]) => v);
  }
  has(name) {
    return this.#pairs.some(([n]) => n === String(name));
  }
  set(name, value) {
    const at = this.#pairs.findIndex(([n]) => n === String(name));
    if (at < 0) {
      this.#pairs.push([String(name), String(value)]);
    } else {
      this.#pairs = this.#pairs.filter(([n], index) => index <= at || n !== String(name));
      this.#pairs[at][1] = String(value);
    }
    this.#touch();
  }
  sort() {
    this.#pairs.sort(([a], [b]) => (a < b ? -1 : a > b ? 1 : 0));
    this.#touch();
  }
  forEach(callback, thisArg) {
    for (const [name, value] of this.#pairs) callback.call(thisArg, value, name, this);
  }
  *entries() {
    yield* this.#pairs.map(([name, value]) => [name, value]);
  }
  *keys() {
    for (const [name] of this.#pairs) yield name;
  }
  *values() {
    for (const [, value] of this.#pairs) yield value;
  }
  [Symbol.iterator]() {
    return this.entries();
  }
  toString() {
    return this.#pairs.map(([name, value]) => `${encodeForm(name)}=${encodeForm(value)}`).join("&");
  }
}

class URL {
  #parts;
  #params;

  constructor(url, base) {
    const against = base === undefined ? undefined : new URL(base).#parts;
    this.#parts = parseUrl(url, against);
    this.#params = new URLSearchParams(this.#parts.search, (query) => {
      this.#parts.search = query ? "?" + query : "";
    });
  }
  static canParse(url, base) {
    try {
      new URL(url, base);
      return true;
    } catch {
      return false;
    }
  }
  get protocol() {
    return this.#parts.protocol;
  }
  get username() {
    return this.#parts.username;
  }
  get password() {
    return this.#parts.password;
  }
  get hostname() {
    return this.#parts.hostname;
  }
  get port() {
    return this.#parts.port;
  }
  get host() {
    return this.#parts.hostname + (this.#parts.port ? ":" + this.#parts.port : "");
  }
  get origin() {
    return this.#parts.protocol in SPECIAL_PORTS ? `${this.#parts.protocol}//${this.host}` : "null";
  }
  get pathname() {
    return this.#parts.pathname;
  }
  set pathname(value) {
    const path = escapePath(String(value));
    this.#parts.pathname = this.#parts.hasAuthority && !path.startsWith("/") ? "/" + path : path;
  }
  get search() {
    return this.#parts.search;
  }
  set search(value) {
    const query = String(value).replace(/^\?/, "");
    this.#parts.search = query ? "?" + escapeQuery(query) : "";
    this.#params = new URLSearchParams(this.#parts.search, (next) => {
      this.#parts.search = next ? "?" + next : "";
    });
  }
  get searchParams() {
    return this.#params;
  }
  get hash() {
    return this.#parts.hash;
  }
  set hash(value) {
    const fragment = String(value).replace(/^#/, "");
    this.#parts.hash = fragment ? "#" + escapeHash(fragment) : "";
  }
  get href() {
    const p = this.#parts;
    const user = p.username ? p.username + (p.password ? ":" + p.password : "") + "@" : "";
    return `${p.protocol}${p.hasAuthority ? "//" + user + this.host : ""}${p.pathname}${p.search}${p.hash}`;
  }
  toString() {
    return this.href;
  }
  toJSON() {
    return this.href;
  }
}

globalThis.URL = URL;
globalThis.URLSearchParams = URLSearchParams;

// Globals: console.

function show(value) {
  if (typeof value === "string") return value;
  if (value instanceof Error) return `${value.name}: ${value.message}${value.stack ? "\n" + value.stack.trimEnd() : ""}`;
  if (typeof value === "function") return `[function ${value.name || "anonymous"}]`;
  try {
    return JSON.stringify(value) ?? String(value);
  } catch {
    return String(value);
  }
}

const log = (level) => (...args) => post({ t: "log", level, message: args.map(show).join(" ") });

globalThis.console = freezeDeep({
  debug: log("debug"),
  log: log("info"),
  info: log("info"),
  warn: log("warn"),
  error: log("error"),
});

// Globals: what Node and browsers have and this runtime doesn't, said plainly.

globalThis.require = (name) => {
  throw new Error(
    `require("${name}") is not available: plugins are ES modules. Import from "diffui" or from a relative file.`,
  );
};
globalThis.fetch = () => {
  throw new Error('fetch is not a global here. Use `http.fetch` from "diffui".');
};

// The Repo handle.

class Repo {
  #ref;

  constructor(handle) {
    this.#ref = { id: handle.id, op: handle.op };
    this.root = handle.root;
    this.forgeRemote = handle.forgeRemote ?? null;
    this.jj = {
      query: (args) => this.#call({ method: "jjQuery", args: words(args, "repo.jj.query") }),
      run: (args) => this.#call({ method: "jjRun", args: words(args, "repo.jj.run") }),
    };
    freezeDeep(this);
  }
  #call(request) {
    return call("repo", { repo: this.#ref, call: request });
  }
  bookmarks() {
    return this.#call({ method: "bookmarks" });
  }
  remotes() {
    return this.#call({ method: "remotes" });
  }
  selection() {
    return this.#call({ method: "selection" });
  }
  rows(revset, options = {}) {
    return this.#call({ method: "rows", revset: String(revset), limit: options.limit ?? 100 });
  }
  diff(revset, options = {}) {
    return this.#call({
      method: "diff",
      revset: String(revset),
      format: options.format ?? "git",
      maxBytes: options.maxBytes ?? 1024 * 1024,
    });
  }
  readFile(rev, path) {
    return this.#call({ method: "readFile", rev: String(rev), path: String(path) });
  }
}

function words(argv, what) {
  if (!Array.isArray(argv) || argv.length === 0 || argv.some((word) => typeof word !== "string")) {
    throw new TypeError(`${what} takes a non-empty array of strings, like ["log", "-r", "@"]`);
  }
  return argv;
}

// Registration. All of it happens while the main module loads, so the host
// knows what a plugin answers before it asks.

let loading = true;
const commandHandlers = new Map();
const listeners = new Map();
let forgeProvider = null;

function whileLoading(what) {
  if (!loading) throw new Error(`${what} must run while the plugin loads (at the top level of its main module), not later`);
}

export const commands = freezeDeep({
  register(id, handler) {
    whileLoading("commands.register");
    if (typeof handler !== "function") throw new TypeError(`commands.register("${id}") needs a function`);
    if (!declaredCommands.has(id)) {
      const known = [...declaredCommands].map((c) => `"${c}"`).join(", ") || "none";
      throw new Error(`Command "${id}" is not declared in plugin.toml. Add a [[commands]] row for it. Declared: ${known}.`);
    }
    if (commandHandlers.has(id)) throw new Error(`Command "${id}" is registered twice`);
    commandHandlers.set(id, handler);
  },
});

export const forge = freezeDeep({
  register(provider) {
    whileLoading("forge.register");
    if (!providesForge) {
      throw new Error('forge.register needs `[provides] forge = { hosts = [...] }` in plugin.toml');
    }
    if (forgeProvider) throw new Error("forge.register was called twice");
    if (!provider || typeof provider.pullRequests !== "function") {
      throw new TypeError("forge.register needs an object with at least a pullRequests(repo, { branches }) method");
    }
    forgeProvider = provider;
  },
});

export const events = freezeDeep({
  on(name, listener) {
    whileLoading("events.on");
    if (!eventNames.includes(name)) {
      throw new Error(`There is no "${name}" event. Events: ${eventNames.join(", ")}.`);
    }
    if (typeof listener !== "function") throw new TypeError(`events.on("${name}") needs a function`);
    if (!listeners.has(name)) listeners.set(name, []);
    listeners.get(name).push(listener);
  },
});

// Host functions.

class Headers {
  #pairs;
  constructor(pairs) {
    this.#pairs = pairs.map(([name, value]) => [name.toLowerCase(), value]);
  }
  get(name) {
    const values = this.#pairs.filter(([n]) => n === String(name).toLowerCase()).map(([, v]) => v);
    return values.length ? values.join(", ") : null;
  }
  has(name) {
    return this.#pairs.some(([n]) => n === String(name).toLowerCase());
  }
  *entries() {
    yield* this.#pairs.map(([name, value]) => [name, value]);
  }
  [Symbol.iterator]() {
    return this.entries();
  }
}

class Response {
  #body;
  constructor(reply) {
    this.status = reply.status;
    this.statusText = reply.statusText;
    this.ok = reply.status >= 200 && reply.status < 300;
    this.url = reply.url;
    this.headers = new Headers(reply.headers);
    this.#body = reply.body;
    Object.freeze(this);
  }
  async text() {
    return this.#body;
  }
  async json() {
    return JSON.parse(this.#body);
  }
}

function requestHeaders(headers) {
  if (headers == null) return [];
  const pairs = typeof headers[Symbol.iterator] === "function" ? [...headers] : Object.entries(headers);
  return pairs.map(([name, value]) => [String(name), String(value)]);
}

export const http = freezeDeep({
  async fetch(url, init = {}) {
    let body = init.body ?? null;
    const headers = requestHeaders(init.headers);
    if (body instanceof URLSearchParams) {
      body = body.toString();
      if (!headers.some(([name]) => name.toLowerCase() === "content-type")) {
        headers.push(["content-type", "application/x-www-form-urlencoded;charset=UTF-8"]);
      }
    }
    if (body !== null && typeof body !== "string") {
      throw new TypeError("http.fetch sends text bodies only: pass a string (JSON.stringify an object)");
    }
    const reply = await call(
      "http.fetch",
      {
        url: String(url),
        method: String(init.method ?? "GET").toUpperCase(),
        headers,
        body,
        timeoutMs: init.timeoutMs ?? null,
      },
      { signal: init.signal },
    );
    return new Response(reply);
  },
});

export async function exec(argv, options = {}) {
  const reply = await call(
    "exec",
    {
      argv: words(argv, "exec"),
      stdin: options.stdin ?? null,
      cwd: options.cwd ?? null,
      timeoutMs: options.timeoutMs ?? null,
      stream: typeof options.onStdout === "function",
    },
    { signal: options.signal, onChunk: options.onStdout },
  );
  return Object.freeze({ ...reply, ok: reply.code === 0 });
}

export const auth = freezeDeep({
  token(host) {
    return call("auth.token", { host: String(host) });
  },
});

export const kv = freezeDeep({
  async get(key) {
    return (await call("kv.get", { key: String(key) })) ?? undefined;
  },
  async set(key, value) {
    if (value === undefined) throw new TypeError("kv.set needs a value; use kv.delete to remove a key");
    await call("kv.set", { key: String(key), value });
  },
  async delete(key) {
    await call("kv.delete", { key: String(key) });
  },
});

export const config = freezeDeep(boot.config);

const uiCall = (request) => call("ui", request);

export const ui = freezeDeep({
  async toast(message, options = {}) {
    await uiCall({ method: "toast", message: String(message), tone: options.tone ?? "info" });
  },
  async activity(label) {
    const activity = await uiCall({ method: "activityStart", label: String(label) });
    return Object.freeze({
      async append(text) {
        await uiCall({ method: "activityAppend", activity, text: String(text) });
      },
      async finish(result = {}) {
        await uiCall({ method: "activityFinish", activity, error: result.error == null ? null : String(result.error) });
      },
    });
  },
  prompt: {
    input(options) {
      return uiCall({
        method: "promptInput",
        label: String(options.label),
        value: options.value ?? null,
        placeholder: options.placeholder ?? null,
      });
    },
    confirm(options) {
      return uiCall({
        method: "promptConfirm",
        title: String(options.title),
        body: options.body ?? null,
        confirmLabel: options.confirmLabel ?? null,
        destructive: Boolean(options.destructive),
      });
    },
    choose(options) {
      const choices = options.choices.map((choice) =>
        typeof choice === "string"
          ? { label: choice, hint: null, value: choice }
          : { label: String(choice.label), hint: choice.hint ?? null, value: String(choice.value ?? choice.label) },
      );
      return uiCall({ method: "promptChoose", label: String(options.label), choices });
    },
  },
  async openUrl(url) {
    await uiCall({ method: "openUrl", url: String(url) });
  },
  clipboard: {
    async write(text) {
      await uiCall({ method: "clipboardWrite", text: String(text) });
    },
  },
  status: {
    async set(text) {
      await uiCall({ method: "statusSet", text: text == null ? null : String(text) });
    },
  },
});

// Calls from the host.

function describe(thrown) {
  if (thrown instanceof Error) {
    return { name: thrown.name, message: thrown.message, stack: thrown.stack ?? "" };
  }
  return { name: "Error", message: `threw ${show(thrown)}`, stack: "" };
}

const commandContext = (c) =>
  Object.freeze({
    repo: new Repo(c.repo),
    changeId: c.changeId ?? null,
    commitId: c.commitId ?? null,
    bookmark: c.bookmark ?? null,
    path: c.path ?? null,
    line: c.line ?? null,
  });

function askForge(repo, request) {
  switch (request.method) {
    case "pullRequests":
      return forgeProvider.pullRequests(repo, { branches: request.branches });
    case "checks":
      return forgeProvider.checks(repo, request.sha);
    case "createPullRequest":
      return forgeProvider.createPullRequest(repo, request.request);
    case "updatePullRequest":
      return forgeProvider.updatePullRequest(repo, request.number, request.patch);
  }
}

function deliver(event) {
  const { name, repo, ...rest } = event;
  const payload = Object.freeze({ ...rest, repo: new Repo(repo) });
  const failed = (thrown) => post({ t: "uncaught", what: `${name} listener`, error: describe(thrown) });
  for (const listener of listeners.get(name) ?? []) {
    try {
      Promise.resolve(listener(payload)).catch(failed);
    } catch (thrown) {
      failed(thrown);
    }
  }
}

async function answer(id, request) {
  try {
    let value = null;
    switch (request.kind) {
      case "command":
        await commandHandlers.get(request.id)(commandContext(request.context));
        break;
      case "forge":
        value = (await askForge(new Repo(request.repo), request.call)) ?? null;
        break;
      case "event":
        deliver(request.event);
        break;
    }
    post({ t: "result", id, ok: true, value });
  } catch (thrown) {
    post({ t: "result", id, ok: false, error: describe(thrown) });
  }
}

boot.dispatch = (json) => {
  const message = JSON.parse(json);
  switch (message.t) {
    case "reply": {
      const pending = pendingCalls.get(message.id);
      if (pending) message.ok ? pending.resolve(message.value) : pending.reject(message.error);
      break;
    }
    case "chunk":
      pendingCalls.get(message.id)?.onChunk?.(message.data);
      break;
    case "timer": {
      const fire = timers.get(message.id);
      timers.delete(message.id);
      fire?.();
      break;
    }
    case "call":
      answer(message.id, message.call);
      break;
  }
};

boot.finishLoad = () => {
  loading = false;
  return JSON.stringify({
    commands: [...commandHandlers.keys()],
    forge: forgeProvider && boot.forgeMethods.filter((method) => typeof forgeProvider[method] === "function"),
    events: [...listeners.keys()],
  });
};
