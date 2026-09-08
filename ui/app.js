// SPDX-FileCopyrightText: 2026 WarpCoreDev
// SPDX-License-Identifier: GPL-3.0-or-later
// Shadow Tentacles — window logic.
//
// Asks Rust for state, draws it, starts jobs and reports their progress. Holds
// no settings of its own: every value the user can change is read from the
// configuration and written back to it.

const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;

const el = (id) => document.getElementById(id);
const all = (selector) => Array.from(document.querySelectorAll(selector));

let busy = false;

// The configured language arrives a moment after the window does; this
// remembers the last answer so the page does not start in English every time.
try {
  setLocale(localStorage.getItem("ui_lang") ?? "en");
} catch (unavailable) {
  setLocale("en");
}

// The languages the window itself is written in. Each is named in itself: a
// reader looking for their own language is not helped by its English name.
el("ui-lang").innerHTML = LOCALES.map(
  ([tag, name]) => `<option value="${tag}">${esc(name)}</option>`,
).join("");

// ---------------------------------------------------------------------------
// shell
// ---------------------------------------------------------------------------

function showView(name) {
  hideToast();
  for (const view of all(".view")) view.hidden = view.dataset.view !== name;
  for (const button of all(".nav-item")) {
    button.setAttribute("aria-pressed", String(button.dataset.view === name));
  }
  if (name === "models") {
    loadModels();
    loadServices();
  }
}

function setBusy(state) {
  busy = state;
  for (const id of ["btn-collect", "btn-summarize", "btn-unload", "btn-get-model"]) {
    el(id).disabled = state;
  }
  for (const button of all(".model-buttons button")) button.disabled = state;
  el("btn-cancel").hidden = !state;
  el("progress").hidden = !state;
  if (!state) {
    el("bar").classList.remove("unknown");
    el("bar-fill").style.width = "0%";
  }
}

// What the running job last said it was doing. Only one thing is done with it:
// loading a model cannot be interrupted, so a stop pressed during it has to be
// acknowledged in words rather than by stopping.
let doingNow = null;

let toastTimer = null;

function toast(message, ok = true) {
  const node = el("toast");
  node.textContent = message;
  node.classList.toggle("bad", !ok);
  node.hidden = false;
  clearTimeout(toastTimer);
  toastTimer = setTimeout(hideToast, 10000);
}

// A message about what just happened has no business outliving the screen it
// happened on, and none at all outliving ten seconds.
function hideToast() {
  clearTimeout(toastTimer);
  el("toast").hidden = true;
}

function describe(error) {
  if (typeof error === "string") return error;
  if (!error?.code) return error?.message ?? t("error.unknown");
  return say({ code: error.code, args: { detail: error.detail ?? "" } });
}

// Summaries are model output about text fetched from the open web, and source
// titles come from a file the user edits. Everything reaches the DOM as text.
function esc(value) {
  return String(value ?? "").replace(
    /[&<>"']/g,
    (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" })[c],
  );
}

// ---------------------------------------------------------------------------
// rendering
// ---------------------------------------------------------------------------

function renderOverview(data) {
  // Before anything is written: the rest of this function asks the dictionary
  // for words, and the settings have just said which dictionary that is.
  if (data.ui_lang !== locale) {
    setLocale(data.ui_lang);
    try {
      localStorage.setItem("ui_lang", data.ui_lang);
    } catch (unavailable) {
      // A window that cannot remember still works; it just starts in English.
    }
  }
  el("ui-lang").value = data.ui_lang;

  el("hardware").textContent = data.hardware;
  el("model").textContent = data.model_name ?? t("model.none");
  // The one case that asks something of the reader, and it asks with a button
  // rather than a sentence: without a model there are no summaries, and a
  // fresh install has none.
  const wanted = data.model_name ? null : data.model_hint;
  el("btn-get-model").hidden = !wanted;
  if (wanted) {
    el("btn-get-model").textContent = t("model.get", {
      name: wanted.name,
      size: num(wanted.gigabytes, 1),
    });
    el("btn-get-model").dataset.id = wanted.id;
  }
  el("btn-unload").hidden = !data.model_loaded;

  el("stat-articles").textContent = data.articles;
  el("stat-articles-label").textContent = t("stat.articles", { n: data.articles });
  el("stat-summaries").textContent = data.summaries;
  el("stat-summaries-label").textContent = t("stat.summaries", { n: data.summaries });
  renderWaiting(data);
  renderSkipped(data.verdicts);

  renderSources(data.sources);

  for (const button of all("#langs .chip")) {
    button.setAttribute("aria-pressed", String(button.dataset.lang === data.output_lang));
  }

  const form = el("settings-form");
  form.output_lang.value = data.output_lang;

  // The words live in the rail, beside the buttons that act on them, because
  // they decide what gets collected. Over the feed they read as a search
  // through what is already there, which is the opposite of what they do.
  showTags(data.keywords);
  form.collect_days.value = data.collect_days;
  form.collect_from.value = data.collect_from ?? "";
  form.collect_to.value = data.collect_to ?? "";
  form.summarize_limit.value = data.summarize_limit;
  form.keep_days.value = data.keep_days;
  form.models_dir.value = data.models_dir;
  el("db-usage").textContent = t("db.usage", {
    size: num(data.database_megabytes, 1),
    articles: t("count.articles", { n: data.articles }),
    summaries: t("count.summaries", { n: data.summaries }),
  });
  form.speech_engine.value = data.speech_engine;
  loadVoices(data.speech_voice ?? "");
  form.speech_rate.value = data.speech_rate;
  // Which engine costs what is a fact about the engine, so the sentence lives
  // here with the rest of the words rather than being sent over as prose.
  const caveat = data.speech_engine === "online" ? t("engine.online_caveat") : "";
  el("speech-caveat").textContent = caveat;
  el("speech-caveat").hidden = !caveat;
  el("audio-usage").textContent = data.audio_files
    ? t("audio.usage", {
        n: t("count.recordings", { n: data.audio_files }),
        size: num(data.audio_megabytes, 1),
      })
    : t("audio.none");
  const explicit = Boolean(data.collect_from || data.collect_to);
  form.range_mode.value = explicit ? "explicit" : "rolling";
  syncRangeMode();
  el("config-path").textContent = t("settings.config_file", { path: data.config_path });

  if (data.busy !== busy) setBusy(data.busy);
}

// Everything the gates turned away, in the words a reader would use. The raw
// reason is kept as a tooltip: it is the truth, but "link density 57%" is not
// an explanation to anyone who did not write the check.
const REASON_KEYS = [
  [/^only \d+ characters/, "reason.too_short"],
  [/^link density/, "reason.links"],
  [/^live coverage/, "reason.liveblog"],
  [/^paywall/, "reason.paywall"],
  [/^expected .*detected/, "reason.language"],
  [/^boilerplate marker/, "reason.boilerplate"],
  [/^the model said so/, "reason.model"],
];

function humanReason(reason) {
  if (!reason) return null;
  for (const [pattern, key] of REASON_KEYS) if (pattern.test(reason)) return t(key);
  return reason;
}

// Articles collected, kept, and not written up yet: what pressing the summarize
// button will now go through.
function renderWaiting(data) {
  const line = el("waiting");
  line.hidden = data.pending === 0;
  if (data.pending > 0) {
    // The button is passed by its own key rather than spelt out here, so the
    // sentence cannot quote a label that is not on the screen.
    line.textContent = t("feed.waiting", {
      n: data.pending,
      button: t("action.summarize"),
    });
  }
  showNotes();
}

// The pinned panel exists only for what is in it. With nothing to say it is not
// an empty box in the corner of the screen, it is gone.
function showNotes() {
  el("feed-notes").hidden = el("waiting").hidden && el("skipped").hidden;
}

function renderSkipped(verdicts) {
  const skipped = verdicts.filter((v) => v.status !== "ok");
  const total = skipped.reduce((sum, v) => sum + v.count, 0);
  el("skipped").hidden = total === 0;
  showNotes();
  if (total === 0) return;

  el("skipped").querySelector("summary").textContent = t("skipped.summary", { n: total });

  // Several checks report the same cause with different numbers in it; group
  // them, or the list turns into one line per article.
  const grouped = new Map();
  for (const v of skipped) {
    // A refusal is not a verdict on the article, so it is named by what
    // happened rather than by what the service said — those words are a request
    // id and mean nothing to a reader. They stay in the tooltip.
    const text =
      v.status === "refused"
        ? t("status.refused")
        : (humanReason(v.reason) ?? labelOf("status", v.status));
    const entry = grouped.get(text) ?? { count: 0, raw: v.reason };
    entry.count += v.count;
    grouped.set(text, entry);
  }

  el("verdicts").innerHTML = Array.from(grouped)
    .sort((a, b) => b[1].count - a[1].count)
    .map(
      ([text, { count, raw }]) =>
        `<li><span title="${esc(raw ?? text)}">${esc(text)}</span><span>${count}</span></li>`,
    )
    .join("");
}

function renderSources(sources) {
  el("source-list").innerHTML = sources
    .map((s, i) => {
      const detail = [labelOf("kindlabel", s.kind), s.lang, s.detail].filter(Boolean).join(" · ");
      // A source that did not answer says so on its own row. The server's own
      // words are kept as the tooltip: they are the truth, and they are also
      // not something to put in front of everyone.
      const silent = s.failure
        ? `<span class="warn" title="${esc(s.failure)}">${esc(t("source.silent"))}</span>`
        : "";
      return `
        <li class="${s.enabled ? "" : "off"}">
          <input type="checkbox" data-index="${i}" ${s.enabled ? "checked" : ""}
                 aria-label="${esc(t("source.enable"))}">
          <span class="name">${esc(s.label)}</span>
          <span>${silent}</span>
          <span class="buttons">
            <button class="icon-btn" data-edit="${i}">${esc(t("source.edit"))}</button>
            <button class="icon-btn remove" data-remove="${i}" title="${esc(t("source.remove"))}">${esc(t("source.remove"))}</button>
          </span>
          <span class="detail">${esc(detail)}</span>
        </li>`;
    })
    .join("");

  for (const box of all("#source-list input[type=checkbox]")) {
    box.addEventListener("change", async () => {
      try {
        await invoke("cmd_toggle_source", { index: Number(box.dataset.index), enabled: box.checked });
        refresh();
      } catch (error) {
        toast(describe(error), false);
      }
    });
  }
  for (const button of all("#source-list [data-edit]")) {
    button.addEventListener("click", () => editSource(Number(button.dataset.edit)));
  }
  for (const button of all("#source-list [data-remove]")) {
    button.addEventListener("click", async () => {
      try {
        await invoke("cmd_remove_source", { index: Number(button.dataset.remove) });
        refresh();
      } catch (error) {
        toast(describe(error), false);
      }
    });
  }
}

// One card, as markup. Written apart from the loop that draws a page so that
// appending the next page and drawing the first are the same thing.
function cardHtml(c) {
  return `
      <article class="card" data-card="${c.id}">
        <h3>${esc(c.title)}</h3>
        <div class="meta">
          ${c.source ? `<span>${esc(c.source)}</span>` : ""}
          <span>${esc(c.published ?? t("card.no_date"))}</span>
          <button class="play" data-play="${c.id}" title="${esc(t("card.play_title"))}">${esc(t("card.play"))}</button>
        </div>
        <ul>${c.bullets.map((b) => `<li>${esc(b)}</li>`).join("")}</ul>
        ${c.tags.length ? `<div class="tags">${c.tags.map((t) => `<span>${esc(t)}</span>`).join("")}</div>` : ""}
        <a href="${esc(c.url)}">${esc(c.url)}</a>
      </article>`;
}

// Draw a page of cards. The first page replaces what is there, every page after
// it is added to the end — which is what makes the feed continuous rather than
// sixty articles and a wall.
function drawCards(cards, append) {
  const feed = el("feed");
  if (append) {
    feed.insertAdjacentHTML("beforeend", cards.map(cardHtml).join(""));
  } else {
    feed.innerHTML = cards.map(cardHtml).join("");
  }

  const shown = feed.childElementCount;
  const searching = feedQuery !== "";
  // Two different nothings. "Collect some news" is wrong advice for a reader
  // who has plenty and simply typed a word that matches none of it.
  el("empty").hidden = shown > 0 || searching;
  el("no-matches").hidden = shown > 0 || !searching;

  // A lone card fills the width instead of sitting in the first of several
  // columns with the rest of the screen blank beside it. From two upwards the
  // columns are the point, so they stay.
  feed.classList.toggle("alone", shown === 1);

  // Only the new cards need listeners; the ones already drawn kept theirs.
  for (const button of all("#feed [data-play]")) {
    if (button.dataset.wired) continue;
    button.dataset.wired = "1";
    button.addEventListener("click", () => play(button));
  }

  // A redraw throws away the mark on the card being read along with the card
  // itself, so the bar is told to put it back rather than waiting for the next
  // tick of the poll — which, with nothing playing, never comes.
  if (!append) marked = null;
  drawPlayer();
}

// A link in a webview opens nothing by itself, and following it in place would
// replace the application with a news site. One listener on the feed rather
// than one per card, because the cards are redrawn on every refresh.
el("feed").addEventListener("click", async (event) => {
  const link = event.target.closest("a[href]");
  if (!link) return;
  event.preventDefault();
  try {
    await invoke("cmd_open_url", { url: link.href });
  } catch (error) {
    toast(describe(error), false);
  }
});

// Playback lives in Rust: the audio never enters this window, because the
// element that would play it needs a media plugin a desktop need not have. Rust
// plays one recording at a time; the queue, the bar and the poll are here.
let polling = null;

// What is being read, and what comes after it. Frozen when reading starts: the
// feed underneath is redrawn by a finished run and replaced by the search box,
// and a queue following those would renumber itself mid-article. Scrolling does
// extend it, since the feed only grows downwards.
let queue = [];
let queueAt = 0;
let preparing = null;

function nowReading() {
  return queue[queueAt];
}

/// Start reading from this card, taking everything below it as the queue.
async function readFrom(cardId) {
  const ids = feedCards().map((node) => Number(node.dataset.card));
  const from = ids.indexOf(cardId);
  queue = from === -1 ? [cardId] : ids.slice(from);
  queueAt = 0;
  await speakCurrent();
}

// Everything the feed holds, from the top.
async function readAll() {
  const ids = feedCards().map((node) => Number(node.dataset.card));
  if (!ids.length) return;
  queue = ids;
  queueAt = 0;
  await speakCurrent();
}

async function speakCurrent() {
  const card = nowReading();
  if (card === undefined) return stopReading();

  // Cleared rather than carried over: the bar would otherwise show the last
  // seconds of the article just finished while the next one is being made.
  drawPlayer({ card, playing: true, position: 0, duration: 0 });
  el("player-getting").hidden = false;
  try {
    await invoke("cmd_speak", { card });
    startPolling();
    prepareNext();
  } catch (error) {
    toast(describe(error), false);
    stopReading();
  } finally {
    el("player-getting").hidden = true;
  }
}

// One ahead, no more. Thirty prepared in advance is minutes of synthesis for a
// reader who will stop after the third, and the recording of the one being
// listened to right now is the only one that has to exist yet.
function prepareNext() {
  const next = queue[queueAt + 1];
  if (next === undefined || preparing === next) return;
  preparing = next;
  invoke("cmd_prepare", { card: next }).catch(() => {}).finally(() => {
    if (preparing === next) preparing = null;
  });
}

async function step(by) {
  const to = queueAt + by;
  if (to < 0) return stopReading();
  // Past the end, but the feed may have grown since: scrolling loads more, and
  // reading should carry on into what has arrived rather than stop at whatever
  // happened to be drawn when the reader pressed play.
  if (to >= queue.length) {
    grow();
    if (to >= queue.length) return stopReading();
  }
  queueAt = to;
  await speakCurrent();
}

// Take in any cards that appeared below the last one already queued. Only
// below: a card that arrived above it belongs to a different list — a redrawn
// feed, a changed search — and following that is how "7 of 30" becomes a lie.
function grow() {
  const ids = feedCards().map((node) => Number(node.dataset.card));
  const last = ids.indexOf(queue[queue.length - 1]);
  if (last === -1) return;
  queue.push(...ids.slice(last + 1));
}

function feedCards() {
  return all("#feed .card");
}

async function stopReading() {
  queue = [];
  queueAt = 0;
  if (polling) {
    clearInterval(polling);
    polling = null;
  }
  try {
    await invoke("cmd_playback", { action: "stop" });
  } catch {
    // Nothing was playing, which is the state being asked for anyway.
  }
  drawPlayer({ card: null, playing: false, position: 0, duration: 0 });
}

function startPolling() {
  if (polling) return;
  polling = setInterval(async () => {
    const state = await invoke("cmd_playback_state");
    drawPlayer(state);
    // Rust plays one file and stops; moving to the next one is this window's
    // job. A paused article also reads as "not playing", so the position tells
    // them apart: only a recording that ran out reaches its own duration.
    const ended = state.duration > 0 && state.position >= state.duration && !state.playing;
    if (ended) {
      clearInterval(polling);
      polling = null;
      if (queue.length) step(1);
    }
  }, 400);
}

// Seconds as a clock. A summary runs to minutes, and "420 s left" is a number
// to do arithmetic on rather than a time to read — every player everywhere
// writes this as 7:00, so it is written as 7:00.
function clock(seconds) {
  const m = Math.floor(seconds / 60);
  return `${m}:${String(seconds - m * 60).padStart(2, "0")}`;
}

// The bar, and the mark on the card it is reading.
let lastState = { card: null, playing: false, position: 0, duration: 0 };
// Which card currently carries the mark. Kept so that a poll two and a half
// times a second does not walk three hundred cards to set a class that has not
// changed since the last time it looked.
let marked = null;

function drawPlayer(state) {
  if (state) lastState = state;
  const on = queue.length > 0;
  el("player").hidden = !on;
  document.body.classList.toggle("playing", on);

  const wanted = on ? nowReading() : null;
  if (wanted !== marked) {
    for (const card of feedCards()) {
      card.classList.toggle("reading", Number(card.dataset.card) === wanted);
    }
    marked = wanted;
  }
  if (!on) return;

  const { playing, position, duration } = lastState;
  el("player-fill").style.width = duration > 0 ? `${(position / duration) * 100}%` : "0";
  el("player-toggle").classList.toggle("paused", !playing);
  el("player-toggle").title = t(playing ? "player.pause" : "player.resume");

  const card = feedCards().find((n) => Number(n.dataset.card) === nowReading());
  el("player-title").textContent = card
    ? card.querySelector("h3").textContent
    : t("player.unknown");

  const left = Math.max(0, Math.round(duration - position));
  el("player-where").textContent =
    t("player.where", { at: queueAt + 1, of: queue.length }) +
    (duration > 0 ? ` · ${t("player.left", { t: clock(left) })}` : "");
}

el("player-toggle").addEventListener("click", async () => {
  const action = lastState.playing ? "pause" : "resume";
  drawPlayer(await invoke("cmd_playback", { action }));
  if (action === "resume") startPolling();
});
// Back to the start of this one if it is under way, to the one before if not:
// which is what every player does and what a hand expects.
el("player-prev").addEventListener("click", () => step(lastState.position > 3 ? 0 : -1));
el("player-next").addEventListener("click", () => step(1));
el("player-close").addEventListener("click", () => stopReading());
el("read-all").addEventListener("click", () => readAll());

// The bar names an article; pressing the name goes to it. Without this the
// reader hears something interesting and has no way to find it in three hundred
// cards.
el("player-title").addEventListener("click", () => {
  const card = feedCards().find((n) => Number(n.dataset.card) === nowReading());
  card?.scrollIntoView({ behavior: "smooth", block: "center" });
});

// Space is pause everywhere else; it is pause here. Not while a field has the
// focus, where a space is a space.
document.addEventListener("keydown", (event) => {
  if (event.code !== "Space" || !queue.length) return;
  const typing = document.activeElement;
  if (typing && ["INPUT", "TEXTAREA", "SELECT"].includes(typing.tagName)) return;
  event.preventDefault();
  el("player-toggle").click();
});

async function play(button) {
  const card = Number(button.dataset.play);
  // Pressing the button of the card already being read means pause or resume,
  // the same as pressing it on the bar.
  if (queue.length && card === nowReading()) {
    el("player-toggle").click();
    return;
  }
  await readFrom(card);
}

// The catalogue in a list, and under it whatever can be done with the one
// chosen. Buttons rather than a row of them per model: five models times three
// buttons is fifteen things to read before pressing one.
let models = [];

function renderModels(fetched) {
  models = fetched;
  const list = el("local-list");
  const keep = list.value;
  list.innerHTML = models
    .map(
      (m) =>
        `<option value="${esc(m.id)}"${m.selected ? " selected" : ""}>${esc(m.name)}${
          m.selected ? " · " + esc(t("model.selected")) : ""
        }${m.present ? "" : " · " + esc(t("model.not_here"))}${
          m.suggested ? " · " + esc(t("model.suggested")) : ""
        }</option>`,
    )
    .join("");
  if (keep && models.some((m) => m.id === keep)) list.value = keep;
  showLocalModel();
}

function showLocalModel() {
  const m = models.find((one) => one.id === el("local-list").value);
  if (!m) return;

  el("local-facts").textContent = t("model.facts", {
    params: m.params_b,
    quant: m.quant,
    download: num(m.download_gb, 1),
    memory: num(m.memory_gb, 1),
  });

  // What can be done depends on what is here: a model on the disk is chosen or
  // deleted, one that is not is fetched.
  const buttons = m.present
    ? [
        m.selected ? null : `<button class="primary" data-select="${esc(m.id)}">${esc(t("model.select"))}</button>`,
        `<button class="danger" data-delete="${esc(m.id)}">${esc(t("model.delete"))}</button>`,
      ]
    : [`<button class="primary" data-download="${esc(m.id)}">${esc(t("model.download"))}</button>`];
  el("local-buttons").innerHTML = buttons.filter(Boolean).join("");

  for (const button of all("#local-buttons [data-download]")) {
    button.addEventListener("click", () => start("cmd_download_model", { id: button.dataset.download }));
  }
  for (const button of all("#local-buttons [data-select]")) {
    button.addEventListener("click", async () => {
      // Choosing a local model is also choosing to work locally.
      await save({ provider: "local", model: button.dataset.select });
      loadChoices();
    });
  }
  for (const button of all("#local-buttons [data-delete]")) {
    button.addEventListener("click", async () => {
      try {
        await invoke("cmd_delete_model", { id: button.dataset.delete });
        toast(t("model.deleted"));
        loadModels();
        refresh();
      } catch (error) {
        toast(describe(error), false);
      }
    });
  }
  if (busy) setBusy(true);
}

// ---------------------------------------------------------------------------
// services
// ---------------------------------------------------------------------------

let services = [];

// Names a saved service by where it is and what it runs, so two providers
// offering the same model name do not read as one entry. Known addresses are
// named after the service, anything else after its host.
function serviceLabel(service) {
  const known = Array.from(el("api-preset").options).find(
    (o) => o.value && o.value === service.url,
  );
  let where = known ? known.textContent : service.url;
  if (!known) {
    try {
      where = new URL(service.url).host;
    } catch (notAnUrl) {
      where = service.url;
    }
  }
  return `${where} — ${service.model}`;
}

function renderServices(fetched) {
  services = fetched;
  const list = el("service-list");
  const keep = list.value;
  list.innerHTML = services
    .map(
      (service, i) =>
        `<option value="${i}"${service.selected ? " selected" : ""}>${esc(serviceLabel(service))}${
          service.selected ? " · " + esc(t("model.selected")) : ""
        }</option>`,
    )
    .join("");
  if (keep && services[Number(keep)]) list.value = keep;

  el("service-empty").hidden = services.length > 0;
  list.hidden = services.length === 0;
  showService();
}

function showService() {
  const at = Number(el("service-list").value);
  const service = services[at];
  el("service-buttons").innerHTML = service
    ? [
        service.selected ? null : `<button class="primary" data-use="${at}">${esc(t("model.select"))}</button>`,
        `<button class="ghost" data-try="${at}">${esc(t("models.api_check"))}</button>`,
        `<button class="danger" data-forget="${at}">${esc(t("model.delete"))}</button>`,
      ]
        .filter(Boolean)
        .join("")
    : "";

  for (const button of all("#service-buttons [data-use]")) {
    button.addEventListener("click", async () => {
      const chosen = services[Number(button.dataset.use)];
      // Choosing a service is also choosing not to work locally.
      await save({ provider: "openai", api_url: chosen.url, api_model: chosen.model });
      loadChoices();
    });
  }
  for (const button of all("#service-buttons [data-try]")) {
    button.addEventListener("click", () => checkService(services[Number(button.dataset.try)]));
  }
  for (const button of all("#service-buttons [data-forget]")) {
    button.addEventListener("click", async () => {
      try {
        await invoke("cmd_remove_service", { index: Number(button.dataset.forget) });
        loadChoices();
        refresh();
      } catch (error) {
        toast(describe(error), false);
      }
    });
  }
}

// One short question to the service, because otherwise the only way to find out
// that an address is wrong is to start a run and watch it fail. A saved entry
// keeps its key in the configuration, so `key` is left out and Rust looks it up.
async function checkService(service) {
  const button = el("check-service");
  const was = button.textContent;
  button.textContent = t("models.api_checking");
  button.disabled = true;
  try {
    toast(say(await invoke("cmd_check_service", service)));
  } catch (error) {
    toast(describe(error), false);
  } finally {
    button.textContent = was;
    button.disabled = false;
  }
}

// ---------------------------------------------------------------------------
// data
// ---------------------------------------------------------------------------

// What the reader typed over the feed. Held here rather than read from the box,
// because the feed is also redrawn by a finished run and by a change of
// language, and those must not drop the search.
let feedQuery = "";

// How many cards are asked for at a time. Small enough that the first screen
// arrives at once, large enough that scrolling does not ask on every flick.
const PAGE = 30;

// How much of the feed has been drawn, and whether the end has been reached.
// `feedDone` starts true because until the first page is drawn there is nothing
// to continue from, and asking again would draw every card twice.
let feedShown = 0;
let feedDone = true;
let feedLoading = false;
let feedFilling = false;

async function refresh() {
  try {
    const [overview, cards] = await Promise.all([
      invoke("cmd_overview"),
      invoke("cmd_cards", { limit: PAGE, skip: 0, query: feedQuery }),
    ]);
    renderOverview(overview);
    startFeed(cards);
  } catch (error) {
    toast(describe(error), false);
  }
}

// Redraw only the feed from the top. Typing is not a reason to ask for the
// whole overview.
async function refreshFeed() {
  try {
    startFeed(await invoke("cmd_cards", { limit: PAGE, skip: 0, query: feedQuery }));
  } catch (error) {
    toast(describe(error), false);
  }
}

// The first page: everything drawn so far is replaced, and the count of what
// has been shown starts again — otherwise the next page would be asked for from
// the wrong place and skip articles.
function startFeed(cards) {
  feedShown = cards.length;
  feedDone = cards.length < PAGE;
  drawCards(cards, false);
  showFeedEnd();
  // A tall window can swallow a whole page without a scrollbar, and then
  // nothing would ever come into view to ask for the next one.
  fillTheScreen();
}

async function loadMoreCards() {
  if (feedDone || feedLoading) return;
  feedLoading = true;
  showFeedEnd();
  try {
    const cards = await invoke("cmd_cards", { limit: PAGE, skip: feedShown, query: feedQuery });
    feedShown += cards.length;
    feedDone = cards.length < PAGE;
    drawCards(cards, true);
  } catch (error) {
    // A page that would not load must not leave the foot spinning for ever;
    // the reader can scroll away and back to ask again.
    feedDone = true;
    toast(describe(error), false);
  } finally {
    feedLoading = false;
    showFeedEnd();
  }
}

// The foot shows only while there is more to come.
function showFeedEnd() {
  el("feed-end").hidden = feedDone;
}

// Keeps asking for pages while the foot is still on screen. One page rarely
// fills a maximised window, and a foot that stays put raises no new event, so
// the observer calls this rather than asking for a page itself.
async function fillTheScreen() {
  if (feedFilling) return;
  feedFilling = true;
  try {
    while (!feedDone) {
      const box = el("feed-end").getBoundingClientRect();
      // Asked for a little before it is actually reached, so the next page is
      // usually there by the time the reader gets to the bottom.
      if (box.top > window.innerHeight + 400) return;
      const before = feedShown;
      await loadMoreCards();
      // A page that added nothing means asking again would add nothing either.
      if (feedShown === before) return;
    }
  } finally {
    feedFilling = false;
  }
}

// The foot coming into view is the whole of the gesture: no button, no page
// numbers, and nothing to notice until there is nothing more to show.
new IntersectionObserver((entries) => {
  if (entries.some((e) => e.isIntersecting)) fillTheScreen();
}, { rootMargin: "400px" }).observe(el("feed-end"));

// The search goes to the database, so it waits for a pause in the typing rather
// than asking on every keystroke.
let feedSearchTimer = null;
function searchFeed(text) {
  const query = text.trim();
  clearTimeout(feedSearchTimer);
  feedSearchTimer = setTimeout(() => {
    if (query === feedQuery) return;
    feedQuery = query;
    refreshFeed();
  }, 250);
}

el("feed-search").addEventListener("input", (event) => searchFeed(event.target.value));

// The box's own cross fires `search` rather than `input` in some builds, so
// both are listened for; an unchanged query redraws nothing.
el("feed-search").addEventListener("search", (event) => searchFeed(event.target.value));

// Escape empties it, the way it does in every other search field.
el("feed-search").addEventListener("keydown", (event) => {
  if (event.key === "Escape" && el("feed-search").value !== "") {
    event.preventDefault();
    el("feed-search").value = "";
    searchFeed("");
  }
});

// The voices are a list, not a guess. A free-text box asked the user to know
// names that live on a server they have never seen.
async function loadVoices(selected) {
  const select = el("speech-voice");
  const list = el("voice-list");
  try {
    const voices = await invoke("cmd_voices");

    select.innerHTML =
      `<option value="">${esc(t("voice.auto"))}</option>` +
      voices
        .filter((voice) => voice.present)
        .map(
          (voice) =>
            `<option value="${esc(voice.id)}"${voice.id === selected ? " selected" : ""}>` +
            `${esc(voice.name)}${voice.quality ? ` · ${esc(voice.quality)}` : ""}</option>`,
        )
        .join("");

    const missing = voices.filter((voice) => !voice.present);
    list.innerHTML = missing
      .map(
        (voice) => `
        <li>
          <span class="name">${esc(voice.name)}${voice.quality ? ` · ${esc(voice.quality)}` : ""}</span>
          <span class="buttons model-buttons">
            <button class="primary" data-get-voice="${esc(voice.id)}">${esc(t("model.download"))}</button>
          </span>
          <span class="facts">${
            voice.size_gb ? esc(t("voice.size", { size: num(voice.size_gb * 1000) })) : ""
          }</span>
        </li>`,
      )
      .join("");

    for (const button of all("#voice-list [data-get-voice]")) {
      button.addEventListener("click", () =>
        start("cmd_download_voice", { id: button.dataset.getVoice }),
      );
    }

    const installed = voices.length - missing.length;
    el("voice-hint").textContent = installed
      ? t("voice.installed", { n: installed })
      : t("voice.none_installed");
  } catch (error) {
    el("voice-hint").textContent = describe(error);
    list.innerHTML = "";
  }
}

async function loadServices() {
  try {
    renderServices(await invoke("cmd_services"));
  } catch (error) {
    toast(describe(error), false);
  }
}

// The choice lives across both lists — picking one side unpicks the other — so
// a change to it redraws both. Redrawing only the list that was clicked left
// the other still showing something as chosen.
async function loadChoices() {
  await loadModels();
  await loadServices();
}

async function loadModels() {
  try {
    renderModels(await invoke("cmd_models"));
  } catch (error) {
    toast(describe(error), false);
  }
}

async function save(patch) {
  try {
    await invoke("cmd_save_settings", { patch });
    // Refreshed first, then announced: one of the things a save can change is
    // the language this message is written in.
    await refresh();
    toast(t("toast.saved"));
    return true;
  } catch (error) {
    toast(describe(error), false);
    return false;
  }
}

async function start(command, args = {}) {
  hideToast();
  setBusy(true);
  try {
    await invoke(command, args);
  } catch (error) {
    setBusy(false);
    toast(describe(error), false);
  }
}

// ---------------------------------------------------------------------------
// wiring
// ---------------------------------------------------------------------------

// A placeholder goes while the field is in use and comes back if it was left
// empty. For the tag fields "empty" means the field holds nothing at all: a chip
// in the box counts, or the example would read as a second value beside it.
function placeholderFor(field) {
  // Asked of the field itself rather than of what the tags happen to hold: a
  // box with a chip in it is a box with something in it, whoever put it there.
  if (field.parentElement?.querySelector(".tag")) return "";
  return field.dataset.hint ?? "";
}

function syncPlaceholder(field) {
  field.placeholder = document.activeElement === field ? "" : placeholderFor(field);
}

for (const field of all("input[placeholder], textarea[placeholder]")) {
  field.dataset.hint = field.placeholder;
  field.addEventListener("focus", () => syncPlaceholder(field));
  field.addEventListener("blur", () => syncPlaceholder(field));
}

for (const button of all(".nav-item")) {
  button.addEventListener("click", () => showView(button.dataset.view));
}

el("btn-collect").addEventListener("click", () => start("cmd_collect"));
el("btn-summarize").addEventListener("click", () => start("cmd_summarize"));
el("btn-cancel").addEventListener("click", async () => {
  await invoke("cmd_cancel");
  // llama.cpp reads the whole file and the wrapper exposes no way to abort it,
  // so the flag is set and nothing visible happens for up to half a minute.
  // Saying so is the difference between waiting and pressing a dead button.
  if (doingNow === "job.loading_model") {
    el("progress-label").textContent = t("job.stopping_after_load");
  }
});
el("btn-get-model").addEventListener("click", () =>
  start("cmd_download_model", { id: el("btn-get-model").dataset.id }),
);
el("btn-unload").addEventListener("click", async () => {
  const freed = await invoke("cmd_unload");
  toast(freed ? t("toast.unloaded") : t("toast.not_loaded"));
  refresh();
});

for (const button of all("#langs .chip")) {
  button.addEventListener("click", async () => {
    for (const other of all("#langs .chip")) {
      other.setAttribute("aria-pressed", String(other === button));
    }
    // The language is a setting, not a view filter: summaries are produced in
    // it, so switching here writes the configuration and the feed follows from
    // there. The window never holds a copy of it to ask with.
    await save({ output_lang: button.dataset.lang });
  });
}

// Shows only the fields the selected kind needs, so an RSS source never asks
// for a CSS selector. No service is named here: any service the user can
// describe, they can add.
function syncSourceForm() {
  const kind = el("kind").value;
  for (const label of all("#add-source [data-for]")) {
    label.hidden = !label.dataset.for.split(" ").includes(kind);
  }
  el("kind-hint").textContent = labelOf("hint", kind);
}
el("kind").addEventListener("change", syncSourceForm);
el("cancel-edit").addEventListener("click", stopEditing);
syncSourceForm();

// Which source the form is standing in for, or null when it is a new one. The
// form is one form in two moods rather than two forms: a second copy would be
// the same fifteen fields waiting to disagree with the first.
let editing = null;

// Fill the form from a source and switch it into editing.
async function editSource(index) {
  try {
    const spec = await invoke("cmd_source", { index });
    const form = el("add-source");
    form.kind.value = spec.kind;
    syncSourceForm();

    form.title.value = spec.title ?? "";
    form.url.value = spec.url ?? "";
    form.search_url.value = spec.search_url ?? "";
    form.sitemap_url.value = spec.sitemap_url ?? "";
    form.lang.value = spec.lang ?? "";
    form.min_points.value = spec.min_points ?? "";
    form.links_selector.value = spec.links_selector ?? "";
    form.body_selector.value = spec.body_selector ?? "";
    form.items_path.value = spec.items_path ?? "";
    form.item_url_path.value = spec.item_url_path ?? "";
    form.item_title_path.value = spec.item_title_path ?? "";
    form.item_date_path.value = spec.item_date_path ?? "";
    form.item_score_path.value = spec.item_score_path ?? "";
    form.headers.value = Object.entries(spec.headers ?? {})
      .map(([name, value]) => `${name}: ${value}`)
      .join("\n");

    editing = index;
    showEditingMood();
    // The form lives inside a fold now, and editing has to open it — otherwise
    // clicking "edit" fills in boxes nobody can see.
    const fold = el("add-source-fold");
    fold.open = true;
    // It sits under the list, and on a long list it is off the screen.
    fold.scrollIntoView({ behavior: "smooth", block: "start" });
  } catch (error) {
    toast(describe(error), false);
  }
}

function showEditingMood() {
  const on = editing !== null;
  el("source-form-head").textContent = t(on ? "sources.edit_head" : "sources.add_head");
  el("save-source").textContent = t(on ? "sources.save_button" : "sources.add_button");
  el("cancel-edit").hidden = !on;
  // Placeholders belong to empty fields, and these were just filled in.
  for (const field of all("#add-source input[data-hint], #add-source textarea[data-hint]")) {
    syncPlaceholder(field);
  }
}

function stopEditing() {
  editing = null;
  el("add-source").reset();
  syncSourceForm();
  showEditingMood();
  // Folded away again: the list is what this screen is about, and a form left
  // open under it pushes the list off the top for no reason.
  el("add-source-fold").open = false;
}

// "Name: value" per line, the way a person writes headers down, rather than a
// JSON object they would have to get the punctuation right in.
function parseHeaders(text) {
  const headers = {};
  for (const line of (text ?? "").split("\n")) {
    const at = line.indexOf(":");
    if (at <= 0) continue;
    const name = line.slice(0, at).trim();
    const value = line.slice(at + 1).trim();
    if (name && value) headers[name] = value;
  }
  return headers;
}

el("add-source").addEventListener("submit", async (event) => {
  event.preventDefault();
  const form = new FormData(event.target);
  const value = (name) => (form.get(name) || "").toString().trim() || null;

  // Field names inside a payload object are read by serde exactly as the Rust
  // struct declares them; Tauri's camelCase conversion applies to a command's
  // own arguments, not to what is nested inside one.
  const source = {
    kind: value("kind"),
    title: value("title"),
    url: value("url"),
    // Sent even when blank, unlike every other field: a cleared address is a
    // decision — "this source cannot be searched", "this publisher keeps no
    // map" — and it has to be written down as an empty string, or it would be
    // guessed at again on the next start.
    search_url: (form.get("search_url") || "").toString().trim(),
    sitemap_url: (form.get("sitemap_url") || "").toString().trim(),
    lang: value("lang"),
    min_points: value("min_points") ? Number(value("min_points")) : null,
    links_selector: value("links_selector"),
    body_selector: value("body_selector"),
    items_path: value("items_path"),
    item_url_path: value("item_url_path"),
    item_title_path: value("item_title_path"),
    item_date_path: value("item_date_path"),
    item_score_path: value("item_score_path"),
    headers: parseHeaders(value("headers")),
  };

  try {
    await invoke("cmd_save_source", { index: editing, source });
    const wasEditing = editing !== null;
    stopEditing();
    toast(t(wasEditing ? "toast.source_saved" : "toast.source_added"));
    refresh();
  } catch (error) {
    toast(describe(error), false);
  }
});

// Two ways to say "which period", and only one of them applies at a time.
// Sending both would leave the dates in the file to override the days silently
// the next time someone switched back.
function syncRangeMode() {
  const form = el("settings-form");
  const explicit = form.range_mode.value === "explicit";
  for (const label of all("#settings-form .radio")) {
    const isExplicit = label.querySelector("input[type=radio]").value === "explicit";
    label.classList.toggle("off", isExplicit !== explicit);
    for (const field of label.querySelectorAll(".inline")) field.disabled = isExplicit !== explicit;
  }
}
for (const radio of all("#settings-form input[name=range_mode]")) {
  radio.addEventListener("change", syncRangeMode);
}

el("settings-form").addEventListener("submit", async (event) => {
  event.preventDefault();
  const form = new FormData(event.target);
  const explicit = form.get("range_mode") === "explicit";
  await save({
    ui_lang: form.get("ui_lang"),
    speech_engine: form.get("speech_engine"),
    speech_voice: form.get("speech_voice"),
    speech_rate: Number(form.get("speech_rate")),
    output_lang: form.get("output_lang"),
    collect_days: explicit ? 0 : Number(form.get("collect_days")),
    collect_from: explicit ? form.get("collect_from") : "",
    collect_to: explicit ? form.get("collect_to") : "",
    summarize_limit: Number(form.get("summarize_limit")),
    keep_days: Number(form.get("keep_days")),
  });
});

// The folder is chosen in the system's own chooser, opened by Rust. Closing it
// without picking is an answer too, and nothing happens.
el("pick-models-dir").addEventListener("click", async () => {
  try {
    const chosen = await invoke("cmd_choose_folder", { start: el("models-dir").value });
    if (chosen) await save({ models_dir: chosen });
  } catch (error) {
    toast(describe(error), false);
  }
});

el("reset-models-dir").addEventListener("click", () => save({ models_dir: "" }));

// The address is a list of the services people reach for, and a box for the one
// they do not. Choosing from the list fills the box; typing an address the list
// does not have moves the list to "another address" rather than leaving it
// pointing at a service this is not.
el("api-preset").addEventListener("change", () => {
  const chosen = el("api-preset").value;
  if (chosen) {
    el("api-url").value = chosen;
  } else {
    el("api-url").focus();
  }
});

el("api-url").addEventListener("input", () => {
  const preset = el("api-preset");
  const typed = el("api-url").value.trim();
  const known = Array.from(preset.options).some((o) => o.value && o.value === typed);
  preset.value = known ? typed : "";
});

el("local-list").addEventListener("change", showLocalModel);
el("service-list").addEventListener("change", showService);

// Adding does not select: choosing which model writes the summaries is its own
// act, in the list above.
el("service-form").addEventListener("submit", async (event) => {
  event.preventDefault();
  const form = new FormData(event.target);
  try {
    await invoke("cmd_save_service", {
      url: form.get("api_url"),
      model: form.get("api_model"),
      key: form.get("api_key"),
    });
    event.target.reset();
    el("add-service").open = false;
    toast(t("models.service_added"));
    loadServices();
  } catch (error) {
    toast(describe(error), false);
  }
});

// The form's own check, on what is typed rather than on what is saved: nobody
// wants to write a key down and only then find out whether it was the right one.
el("check-service").addEventListener("click", () => {
  const form = new FormData(el("service-form"));
  checkService({
    url: form.get("api_url"),
    model: form.get("api_model"),
    key: form.get("api_key"),
  });
});

el("ui-lang").addEventListener("change", (event) => save({ ui_lang: event.target.value }));

// Switching engines changes what a voice even is, so the list is rebuilt —
// after saving, because the list is asked for by the settings, not by the form.
el("settings-form").speech_engine.addEventListener("change", async (event) => {
  await save({ speech_engine: event.target.value });
  loadVoices("");
});

// ---------------------------------------------------------------------------
// keywords as tags
// ---------------------------------------------------------------------------
//
// One list is stored, an excluded subject carrying a leading minus, which is the
// format of the configuration file. The window splits it into two fields so that
// exclusion is something to see rather than punctuation to remember.

let tags = { include: [], exclude: [] };

function splitKeywords(keywords) {
  return {
    include: keywords.filter((k) => !k.startsWith("-")),
    exclude: keywords.filter((k) => k.startsWith("-")).map((k) => k.slice(1).trim()),
  };
}

function joinKeywords(state) {
  return [...state.include, ...state.exclude.map((t) => `-${t}`)];
}

function showTags(keywords) {
  const next = splitKeywords(keywords);
  // Only redraw when the set actually changed: a redraw during typing would
  // take the caret with it.
  if (JSON.stringify(next) !== JSON.stringify(tags)) {
    tags = next;
    drawTags("include");
    drawTags("exclude");
  }
  // Outside that check, because switching the window's language puts every
  // placeholder back — including into a field that already holds tags.
  syncPlaceholder(el("include-input"));
  syncPlaceholder(el("exclude-input"));
}

function drawTags(which) {
  const field = el(`${which}-field`);
  const input = el(`${which}-input`);
  for (const chip of Array.from(field.querySelectorAll(".tag"))) chip.remove();

  for (const [i, text] of tags[which].entries()) {
    const chip = document.createElement("span");
    chip.className = "tag";
    chip.textContent = text;
    const remove = document.createElement("button");
    remove.type = "button";
    remove.textContent = "×";
    remove.title = t("tags.remove");
    remove.addEventListener("click", () => {
      tags[which].splice(i, 1);
      drawTags(which);
      saveTags();
    });
    chip.appendChild(remove);
    field.insertBefore(chip, input);
  }

  syncPlaceholder(input);

  el("keyword-hint").textContent = tags.include.length
    ? t("tags.summary", { n: tags.include.length })
    : t("tags.empty");
}

// Saved without a toast: adding a tag is not an event worth announcing, and
// the feed redrawing underneath is the confirmation.
async function saveTags() {
  try {
    await invoke("cmd_save_settings", { patch: { keywords: joinKeywords(tags) } });
    await refresh();
  } catch (error) {
    toast(describe(error), false);
  }
}

function addTags(which, text) {
  const wanted = text
    .split(",")
    .map((t) => t.trim().replace(/^-+/, "").replace(/\s+/g, " "))
    .filter(Boolean)
    .filter((t) => !tags[which].includes(t));
  if (wanted.length === 0) return false;
  tags[which].push(...wanted);
  drawTags(which);
  saveTags();
  return true;
}

for (const which of ["include", "exclude"]) {
  const input = el(`${which}-input`);

  // A comma anywhere in what was just typed — or pasted — ends a tag.
  input.addEventListener("input", () => {
    if (!input.value.includes(",")) return;
    const parts = input.value.split(",");
    const tail = parts.pop();
    addTags(which, parts.join(","));
    input.value = tail.trim();
  });

  input.addEventListener("keydown", (event) => {
    if (event.key === "Enter") {
      event.preventDefault();
      if (addTags(which, input.value)) input.value = "";
      return;
    }
    // Backspace in an empty box takes back the last tag, the way every other
    // tag field behaves.
    if (event.key === "Backspace" && input.value === "" && tags[which].length) {
      tags[which].pop();
      drawTags(which);
      saveTags();
    }
  });

  // Half-typed text must not be lost by clicking elsewhere.
  input.addEventListener("blur", () => {
    if (addTags(which, input.value)) input.value = "";
  });

  // The whole field is one control: clicking its padding focuses the input.
  el(`${which}-field`).addEventListener("click", (event) => {
    if (event.target === el(`${which}-field`)) input.focus();
  });
}

// Two presses rather than a dialog: a confirm() belongs to the host browser,
// and this deletes everything the program has collected.
// The label is read at the moment of the press rather than when this is wired
// up: the window can change language in between, and a button that came back
// from being armed in yesterday's language would be a small lie.
function arm(button, action) {
  button.addEventListener("click", async () => {
    if (button.dataset.armed !== "yes") {
      button.dataset.armed = "yes";
      button.dataset.was = button.textContent;
      button.textContent = t("confirm.again");
      setTimeout(() => {
        button.dataset.armed = "no";
        button.textContent = button.dataset.was;
      }, 4000);
      return;
    }
    button.dataset.armed = "no";
    button.textContent = button.dataset.was;
    try {
      toast(say(await action()));
      refresh();
    } catch (error) {
      toast(describe(error), false);
    }
  });
}

arm(el("clear-db"), () => invoke("cmd_clear_all"));

// Going over every stored article takes a moment and touches nothing outside
// this machine, so the button simply waits rather than becoming a job with a
// progress bar of its own — but it must not look dead while it thinks.
el("recheck-now").addEventListener("click", async () => {
  const button = el("recheck-now");
  button.disabled = true;
  try {
    toast(say(await invoke("cmd_recheck")));
    refresh();
  } catch (error) {
    toast(describe(error), false);
  } finally {
    button.disabled = false;
  }
});

el("prune-now").addEventListener("click", async () => {
  try {
    toast(say(await invoke("cmd_prune")));
    refresh();
  } catch (error) {
    toast(describe(error), false);
  }
});

el("clear-audio").addEventListener("click", async () => {
  const removed = await invoke("cmd_clear_audio");
  toast(removed ? t("audio.deleted", { n: removed }) : t("audio.nothing"));
  refresh();
});

listen("job://progress", ({ payload }) => {
  // A total of zero means the length is not knowable, not that there is
  // nothing to do — the bar sweeps instead of pretending to a number.
  doingNow = payload.message.code;
  const unknown = !payload.total;
  el("bar").classList.toggle("unknown", unknown);
  el("bar-fill").style.width = unknown
    ? ""
    : `${Math.round((payload.done / payload.total) * 100)}%`;

  // Some messages count for themselves: a download says how many gigabytes of
  // how many. Putting the same fraction in front of those reads as two numbers
  // arguing.
  const args = payload.message.args ?? {};
  const counts = "total" in args;
  const ahead = payload.total > 1 && !counts ? `${payload.done}/${payload.total} — ` : "";
  // The rate arrives only once there is one to report: not at the start of a
  // download, and not while the file is being checked afterwards.
  const rate = "speed" in args ? ` · ${t("job.speed", { speed: args.speed })}` : "";
  el("progress-label").textContent = ahead + say(payload.message) + rate;
});

// A batch has been written down. Show it now rather than at the end of a run
// that may have two hundred more articles to go.
listen("job://batch", () => refresh());

listen("job://done", ({ payload }) => {
  setBusy(false);
  toast(joinParts(payload.parts), payload.ok);
  refresh();
  if (payload.phase === "download") {
    loadModels();
    loadVoices(el("speech-voice").value);
  }
});

refresh();
