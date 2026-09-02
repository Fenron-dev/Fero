"use strict";

/* Fero — Oberfläche.
 *
 * Vier Ansichten: Abos, Abo-Detail, Einstellungen, Protokoll. Kein Framework
 * und kein Bundler; die CSP erlaubt ohnehin nur eigene Skripte, und ein
 * Beschaffungswerkzeug soll sofort da sein.
 *
 * Grundregel für Text aus dem Netz: er wird ausschliesslich über textContent
 * gesetzt, nie über innerHTML. Titel und Beschreibungen kommen von fremden
 * Seiten und sind nicht vertrauenswürdig.
 */

const API = "fero://localhost/api";

/* Medientypen kommen vom Backend (MediaKind::ALL), damit ein neuer Typ hier
 * nichts zu ändern verlangt. Bis /api/targets geantwortet hat, steht die Liste
 * leer — die Oberfläche wartet darauf, statt Typen fest zu verdrahten. */
let mediaKinds = [];

let subscriptions = [];
let currentDetailId = null;
let jobTimer = null;
let dataDir = null;

// ── Kleinkram ────────────────────────────────────────────────────────────

const $ = (id) => document.getElementById(id);
const el = (tag, className, text) => {
  const node = document.createElement(tag);
  if (className) node.className = className;
  if (text !== undefined) node.textContent = text;
  return node;
};
const clear = (node) => {
  while (node.firstChild) node.removeChild(node.firstChild);
};

function setFeedback(node, message, kind) {
  if (!node) return;
  node.textContent = message || "";
  node.className = "feedback" + (kind ? " " + kind : "");
}

let statusTimer = null;
function status(message, isError) {
  const strip = $("status-strip");
  strip.textContent = message;
  strip.className = "status-strip" + (isError ? " error" : "");
  strip.hidden = false;
  clearTimeout(statusTimer);
  statusTimer = setTimeout(() => {
    strip.hidden = true;
  }, isError ? 8000 : 3500);
}

/* Einziger Zugang zum Backend. Fehler landen an genau einer Stelle, damit
 * nicht jeder Aufrufer sein eigenes Fehlerverhalten erfindet. */
async function api(path, options) {
  const response = await fetch(`${API}/${path}`, options);
  /* Erst den Body lesen: seit die API echte Statuscodes liefert, steht die
   * verstaendliche Fehlermeldung im JSON — "400 Bad Request" allein hilft
   * niemandem. Der Statustext ist nur der letzte Ausweg. */
  let data = null;
  try {
    data = await response.json();
  } catch (ignored) {
    /* kein JSON — unten faellt es auf den Statustext zurueck */
  }
  if (data && data.error) throw new Error(data.error);
  if (!response.ok) {
    throw new Error(`${response.status} ${response.statusText}`);
  }
  return data;
}

const post = (path, payload) =>
  api(path, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(payload),
  });

/** Fragt den Nutzer nach einem Ordner. Gibt null zurück, wenn er abbricht. */
async function pickFolder() {
  const data = await api("select-folder");
  return data.path || null;
}

/* Eigener Bestaetigungsdialog. window.confirm liefert in Tauris WebView
 * sofort "abgebrochen" zurueck — jede damit abgesicherte Aktion tat schlicht
 * nichts. Der Dialog hier ist eigenes DOM und funktioniert ueberall. */
function confirmAction(message, confirmLabel) {
  return new Promise((resolve) => {
    const overlay = el("div", "confirm-overlay");
    const box = el("div", "confirm-box");
    box.appendChild(el("p", "confirm-text", message));

    const row = el("div", "row");
    const ok = el("button", "action danger", confirmLabel || "Ja, fortfahren");
    const cancel = el("button", "action", "Abbrechen");
    row.appendChild(ok);
    row.appendChild(cancel);
    box.appendChild(row);
    overlay.appendChild(box);

    const close = (answer) => {
      overlay.remove();
      resolve(answer);
    };
    ok.addEventListener("click", () => close(true));
    cancel.addEventListener("click", () => close(false));
    overlay.addEventListener("click", (event) => {
      if (event.target === overlay) close(false);
    });

    document.body.appendChild(overlay);
    cancel.focus();
  });
}

// ── Navigation ───────────────────────────────────────────────────────────

function showView(name) {
  document.querySelectorAll(".view").forEach((view) => {
    view.classList.toggle("is-active", view.dataset.view === name);
  });
  document.querySelectorAll(".nav-item").forEach((item) => {
    item.classList.toggle("is-active", item.dataset.view === name);
  });
}

document.querySelectorAll(".nav-item").forEach((item) => {
  item.addEventListener("click", () => {
    const view = item.dataset.view;
    showView(view);
    if (view === "settings") {
      loadTargets();
      loadSchedule();
      loadBlocklist();
    }
    if (view === "trash") loadTrash();
    if (view === "log") loadLog();
  });
});

$("detail-back").addEventListener("click", () => {
  currentDetailId = null;
  showView("subscriptions");
});

// ── Abos laden und anzeigen ──────────────────────────────────────────────

/** Holt beide Listen parallel und markiert jeden Eintrag mit seinem Typ. */
async function loadSubscriptions() {
  const [novels, mangas] = await Promise.all([
    api("webnovel/list").catch((error) => ({ subscriptions: [], loadError: error.message })),
    api("manga/list").catch((error) => ({ subscriptions: [], loadError: error.message })),
  ]);

  const problem = novels.loadError || mangas.loadError;
  if (problem) status(problem, true);

  subscriptions = [
    ...(novels.subscriptions || []).map((item) => ({
      ...item,
      kind: "webnovel",
      mediaKind: item.mediaKind || "webnovel",
    })),
    ...(mangas.subscriptions || []).map((item) => ({
      ...item,
      kind: "manga",
      mediaKind: item.mediaKind || "manga",
    })),
  ].sort((a, b) => a.title.localeCompare(b.title, "de"));

  $("nav-count-subscriptions").textContent = subscriptions.length || "";
  renderSubscriptions();
}

/* Der ermittelte Serienstatus. Drei davon aendern, was jemand tun wuerde:
 * lizenziert heisst "jetzt laden, naechste Woche ist es vielleicht weg",
 * abgebrochen heisst "wird nie fertig", Hiatus heisst "nicht kaputt, nur
 * still". Die werden deshalb hervorgehoben. */
const STATUS_LABELS = {
  ongoing: { text: "laufend", tone: "" },
  completed: { text: "abgeschlossen", tone: "ok" },
  hiatus: { text: "Hiatus", tone: "warn" },
  dropped: { text: "abgebrochen", tone: "warn" },
  licensed: { text: "lizenziert", tone: "warn" },
  unknown: { text: "", tone: "" },
};

const STATUS_HINTS = {
  licensed: "Lizenziert — Fan-Übersetzungen verschwinden danach oft innerhalb von Tagen. Jetzt vollständig herunterladen.",
  dropped: "Von der Übersetzergruppe abgebrochen. Möglicherweise übernimmt eine andere Gruppe.",
  hiatus: "Pausiert — seit längerem keine neuen Kapitel. Fero sieht weiter nach, nur seltener; kommt etwas, gilt die Serie wieder als laufend.",
};

/* „vor 3 Tagen" statt eines Datums, das man erst im Kopf verrechnen muss.
 * Tage sind die feinste sinnvolle Einheit: die Quellen nennen selbst keine
 * Uhrzeit, alles Genauere waere erfunden. */
function sinceLabel(unix) {
  const days = Math.floor((Date.now() / 1000 - unix) / 86400);
  if (days <= 0) return "heute";
  if (days === 1) return "gestern";
  if (days < 30) return `vor ${days} Tagen`;
  const months = Math.round(days / 30.4);
  if (months < 12) return months === 1 ? "vor einem Monat" : `vor ${months} Monaten`;
  const years = Math.round(days / 365.25);
  return years === 1 ? "vor einem Jahr" : `vor ${years} Jahren`;
}

function statusBadge(status) {
  const entry = STATUS_LABELS[status];
  if (!entry || !entry.text) return null;
  return el("span", "badge " + entry.tone, entry.text);
}

/* Kategorie -> Engine. Die H-Kategorien sind eine Regal-Entscheidung, keine
 * technische: ein H/Manga wird von der Manga-Engine geholt. */
const engineFor = (mediaKind) =>
  mediaKind === "manga" || mediaKind === "hmanga" ? "manga" : "webnovel";

/* Cover-URL; der letzte Prueflauf dient als Cache-Brecher, damit ein neues
 * Cover ankommt, unveraenderte aber aus dem Cache kommen. */
const coverUrl = (item) =>
  `${API}/cover?id=${encodeURIComponent(item.id)}&kind=${item.mediaKind}&t=${item.lastCheckUnix || 0}`;

let viewMode = localStorage.getItem("fero.viewMode") || "list";

function setViewMode(mode) {
  viewMode = mode;
  localStorage.setItem("fero.viewMode", mode);
  $("view-list").classList.toggle("is-active", mode === "list");
  $("view-grid").classList.toggle("is-active", mode === "grid");
  renderSubscriptions();
}

function kindLabel(id) {
  const kind = mediaKinds.find((entry) => entry.id === id);
  return kind ? kind.label : id;
}

/* Sortierungen der Abo-Liste. Der Vergleich liefert immer eine Zahl, damit ein
 * fehlendes Feld (nie geprueft, kein Datum) hinten landet statt die Reihenfolge
 * zufaellig zu machen. */
const SORTERS = {
  added: (a, b) => (b.createdAtUnix || 0) - (a.createdAtUnix || 0),
  title: (a, b) => a.title.localeCompare(b.title, "de"),
  release: (a, b) => (b.latestReleaseUnix || 0) - (a.latestReleaseUnix || 0),
  checked: (a, b) => (b.lastCheckUnix || 0) - (a.lastCheckUnix || 0),
  progress: (a, b) =>
    (b.knownChapters - b.downloadedChapters) - (a.knownChapters - a.downloadedChapters),
  status: (a, b) =>
    (STATUS_ORDER[statusKey(a)] ?? 9) - (STATUS_ORDER[statusKey(b)] ?? 9) ||
    a.title.localeCompare(b.title, "de"),
};

/* Reihenfolge nach Dringlichkeit, nicht alphabetisch: was Aufmerksamkeit
 * braucht, steht oben. */
const STATUS_ORDER = {
  licensed: 0,
  ongoing: 1,
  unknown: 2,
  hiatus: 3,
  dropped: 4,
  completed: 5,
  paused: 6,
};

/* Der eine Zustand, den ein Abo im Blick der Liste hat. „Pausiert" schlaegt
 * alles: ein pausiertes Abo wird nicht geprueft, egal was die Quelle sagt. */
function statusKey(item) {
  if (!item.enabled) return "paused";
  return item.seriesStatus || "unknown";
}

function visibleSubscriptions() {
  const needle = $("filter-input").value.trim().toLowerCase();
  const kind = $("kind-filter").value;
  const state = $("state-filter").value;
  const items = subscriptions.filter((item) => {
    if (kind && item.mediaKind !== kind) return false;
    if (state === "paused" && item.enabled) return false;
    if (state === "active" && !item.enabled) return false;
    if (state && state !== "paused" && state !== "active" && statusKey(item) !== state) {
      return false;
    }
    if (!needle) return true;
    return (
      item.title.toLowerCase().includes(needle) ||
      item.source.toLowerCase().includes(needle)
    );
  });
  const sorter = SORTERS[$("sort-select").value] || SORTERS.added;
  return items.sort(sorter);
}

/* Eine Kachel im Raster: Cover (oder Initiale), Titel darunter — zum Finden
 * nach dem Bild statt nach dem Namen. */
function gridTile(item) {
  const tile = el("button", "tile" + (item.enabled ? "" : " is-paused"));
  tile.type = "button";
  if (!item.enabled) {
    // Im Raster gibt es keine Textzeile fuer Abzeichen — das Zeichen muss auf
    // dem Bild sitzen, sonst sieht man den Zustand nur in der Liste.
    tile.appendChild(el("span", "tile-flag", "⏸"));
  }
  if (item.hasCover) {
    const img = el("img");
    img.src = coverUrl(item);
    img.loading = "lazy";
    img.alt = "";
    tile.appendChild(img);
  } else {
    tile.appendChild(el("div", "tile-fallback", item.title.slice(0, 1)));
  }
  tile.appendChild(el("div", "tile-title", item.title));
  tile.appendChild(
    el(
      "div",
      "tile-meta",
      `${kindLabel(item.mediaKind)} · ${item.downloadedChapters}/${item.knownChapters}`
    )
  );
  tile.addEventListener("click", () => openDetail(item.id));
  return tile;
}

function renderSubscriptions() {
  const list = $("subscription-list");
  const empty = $("subscription-empty");
  clear(list);
  list.className = "cards" + (viewMode === "grid" ? " grid" : "");

  const items = visibleSubscriptions();
  if (items.length === 0) {
    empty.hidden = false;
    empty.textContent = subscriptions.length
      ? "Keine Treffer für diesen Filter."
      : "Noch keine Abos. Oben rechts eins hinzufügen.";
    return;
  }
  empty.hidden = true;

  if (viewMode === "grid") {
    for (const item of items) {
      list.appendChild(gridTile(item));
    }
    return;
  }

  for (const item of items) {
    const card = el("button", "card" + (item.enabled ? "" : " is-paused"));
    card.type = "button";

    if (item.hasCover) {
      const thumb = el("img", "card-thumb");
      thumb.src = coverUrl(item);
      thumb.loading = "lazy";
      thumb.alt = "";
      card.appendChild(thumb);
    } else {
      card.appendChild(el("div", "card-thumb fallback", item.title.slice(0, 1)));
    }

    const left = el("div");
    left.appendChild(el("div", "card-title", item.title));
    left.appendChild(
      el("div", "card-meta", `${kindLabel(item.mediaKind)} · ${item.source}`)
    );

    const badges = el("div", "badges");
    if (!item.enabled) badges.appendChild(el("span", "badge off", "⏸ pausiert"));
    // Der ermittelte Status schlaegt die Handschalter; nur solange keine Quelle
    // befragt wurde, zaehlen completed/hiatus.
    const badge = statusBadge(item.seriesStatus);
    if (badge) {
      badges.appendChild(badge);
    } else {
      if (item.completed) badges.appendChild(el("span", "badge ok", "abgeschlossen"));
      if (item.hiatus) badges.appendChild(el("span", "badge warn", "Hiatus"));
    }
    if (item.lastError) badges.appendChild(el("span", "badge warn", "Fehler"));
    if (badges.childElementCount) left.appendChild(badges);

    const right = el(
      "div",
      "card-progress",
      `${item.downloadedChapters} / ${item.knownChapters}`
    );

    card.appendChild(left);
    card.appendChild(right);
    card.addEventListener("click", () => openDetail(item.id));
    list.appendChild(card);
  }
}

$("view-list").addEventListener("click", () => setViewMode("list"));
$("view-grid").addEventListener("click", () => setViewMode("grid"));

$("filter-input").addEventListener("input", renderSubscriptions);
$("kind-filter").addEventListener("change", renderSubscriptions);
$("state-filter").addEventListener("change", renderSubscriptions);
$("sort-select").addEventListener("change", () => {
  localStorage.setItem("fero.sort", $("sort-select").value);
  renderSubscriptions();
});
$("sort-select").value = localStorage.getItem("fero.sort") || "added";

// ── Abo hinzufügen ───────────────────────────────────────────────────────

$("add-open").addEventListener("click", () => {
  $("add-panel").hidden = false;
  $("add-url").focus();
});
$("add-cancel").addEventListener("click", () => {
  $("add-panel").hidden = true;
  setFeedback($("add-feedback"), "");
});

$("add-submit").addEventListener("click", async () => {
  const url = $("add-url").value.trim();
  const kind = $("add-kind").value;
  if (!url) {
    setFeedback($("add-feedback"), "Bitte eine URL angeben.", "error");
    return;
  }
  const button = $("add-submit");
  button.disabled = true;
  setFeedback($("add-feedback"), "Quelle wird gelesen …");
  try {
    const result = await post(`${engineFor(kind)}/subscribe`, { url, mediaKind: kind });
    if (result.alreadySubscribed) {
      setFeedback($("add-feedback"), "Diese URL ist bereits abonniert.", "error");
    } else {
      $("add-url").value = "";
      $("add-panel").hidden = true;
      setFeedback($("add-feedback"), "");
      status(`„${result.subscription.title}" abonniert.`);
      await loadSubscriptions();
    }
  } catch (error) {
    setFeedback($("add-feedback"), error.message, "error");
  } finally {
    button.disabled = false;
  }
});

// ── Detailansicht ────────────────────────────────────────────────────────

function fact(list, term, value) {
  if (value === undefined || value === null || value === "") return;
  list.appendChild(el("dt", null, term));
  list.appendChild(el("dd", null, String(value)));
}

function openDetail(id) {
  const item = subscriptions.find((entry) => entry.id === id);
  if (!item) return;
  currentDetailId = id;

  $("detail-title").textContent = item.title;

  const facts = $("detail-facts");
  clear(facts);
  fact(facts, "Typ", kindLabel(item.mediaKind));
  fact(facts, "Quelle", item.source);
  fact(facts, "Autor", item.author);
  fact(facts, "Kapitel", `${item.downloadedChapters} von ${item.knownChapters} geladen`);
  const statusEntry = STATUS_LABELS[item.seriesStatus];
  fact(
    facts,
    "Status",
    statusEntry && statusEntry.text
      ? statusEntry.text
      : item.completed
        ? "abgeschlossen"
        : item.hiatus
          ? "Hiatus"
          : "laufend"
  );
  if (item.statusOverride) {
    fact(facts, "Status kommt von", "Handeinstellung");
  } else if (item.statusCheckedAt) {
    fact(facts, "Status geprüft", new Date(item.statusCheckedAt * 1000).toLocaleDateString("de-DE"));
  }
  if (item.translationDone !== undefined && item.translationDone !== null) {
    fact(facts, "Übersetzung", item.translationDone ? "vollständig" : "läuft noch");
  }
  if (item.latestReleaseUnix) {
    // Die Frage dahinter ist nicht das Datum, sondern „wie lange liegt das
    // schon" — deshalb steht der Abstand daneben.
    const date = new Date(item.latestReleaseUnix * 1000);
    fact(
      facts,
      "Neuestes Kapitel bei der Quelle",
      `${date.toLocaleDateString("de-DE")} (${sinceLabel(item.latestReleaseUnix)})`
    );
  }
  if (item.lastCheckUnix) {
    fact(facts, "Letzter Lauf", new Date(item.lastCheckUnix * 1000).toLocaleString("de-DE"));
  }
  if (item.lastError) fact(facts, "Letzter Fehler", item.lastError);
  if (item.ratingExternal) fact(facts, "Bewertung", `${item.ratingExternal} / 5`);
  if (item.downloadLimit) fact(facts, "Kapitel-Limit", `${item.downloadLimit}`);
  if (item.deliveredTo) fact(facts, "Dateien liegen in", item.deliveredTo);

  const cover = $("detail-cover");
  if (item.hasCover) {
    /* Zeitstempel gegen den Bild-Cache: nach einem Prüflauf kann ein neues
     * Cover liegen, die URL wäre sonst dieselbe. */
    cover.src = coverUrl(item);
    cover.hidden = false;
  } else {
    cover.hidden = true;
    cover.removeAttribute("src");
  }

  const banner = $("detail-status-banner");
  const hint = STATUS_HINTS[item.seriesStatus];
  banner.textContent = hint || "";
  banner.hidden = !hint;

  $("detail-description").textContent = item.description || "";

  const genres = $("detail-genres");
  clear(genres);
  for (const genre of item.genres || []) {
    genres.appendChild(el("span", "badge", genre));
  }

  renderDetailTarget(item);
  fillKindSelect(item);
  setFeedback($("detail-feedback"), "");
  setFeedback($("detail-target-feedback"), "");
  setFeedback($("detail-login-feedback"), "");
  setFeedback($("detail-status-feedback"), "");
  $("detail-rebuild").hidden = item.kind !== "webnovel";
  $("detail-anilist").hidden = !item.anilistUrl;
  $("detail-mal").hidden = !item.malUrl;
  $("detail-pause").checked = !item.enabled;
  renderDetailStatus(item);
  refreshLoginState();
  showView("detail");
}

function renderDetailTarget(item) {
  const node = $("detail-target-current");
  if (item.targetDir) {
    node.textContent = `Eigener Ordner: ${item.targetDir}`;
  } else {
    node.textContent = `Standard für ${kindLabel(item.mediaKind)} wird verwendet.`;
  }
  $("detail-clear-target").disabled = !item.targetDir;

  /* Liegen die Dateien woanders, als die Zielkette zeigt — lokal gesammelt,
   * weil das Netzziel offline war, oder die Kategorie hat gewechselt — gibt es
   * genau hier den einen Knopf, der sie hintraegt. */
  const info = $("detail-relocate-info");
  const button = $("detail-relocate");
  if (item.needsRelocation) {
    info.textContent =
      `Die Dateien liegen derzeit in ${item.deliveredTo} — das aktuelle Ziel ist ein anderer Ordner.`;
    button.hidden = false;
  } else {
    info.textContent = "";
    button.hidden = true;
  }
}

function currentItem() {
  return subscriptions.find((entry) => entry.id === currentDetailId);
}

/* Die Statusquelle unterscheidet sich je Engine: Webnovels lesen eine
 * NovelUpdates-Seite, Manga einen AniList- oder MyAnimeList-Eintrag. Der
 * Handschalter darueber gilt fuer beide gleich. */
const STATUS_SOURCE = {
  webnovel: {
    placeholder: "https://www.novelupdates.com/series/…",
    hint:
      "Statusquelle: die NovelUpdates-Seite dieser Serie. Daraus liest Fero, ob sie " +
      "noch läuft, fertig übersetzt oder lizenziert ist. Bei einem NovelUpdates-Abo " +
      "wird sie automatisch verwendet.",
  },
  manga: {
    placeholder: "https://anilist.co/manga/… oder https://myanimelist.net/manga/…",
    hint:
      "Statusquelle: der AniList- oder MyAnimeList-Eintrag dieser Serie. Fero sucht " +
      "ihn selbst, übernimmt aber nur einen Treffer, dessen Titel wirklich passt — " +
      "Scanlation- und Datenbanktitel weichen oft voneinander ab. Findet er nichts " +
      "Sicheres, steht das im Protokoll und der Link gehört hierher.",
  },
};

function renderDetailStatus(item) {
  const source = STATUS_SOURCE[item.kind] || STATUS_SOURCE.manga;
  clear($("detail-search-hits"));
  $("detail-search-query").value = "";
  $("detail-status").value = item.statusOverride || "auto";
  $("detail-status-source-hint").textContent = source.hint;
  const field = $("detail-status-url");
  field.placeholder = source.placeholder;
  field.value = item.statusSourceUrl || "";
}

$("detail-status-save").addEventListener("click", async () => {
  const item = currentItem();
  if (!item) return;
  const node = $("detail-status-feedback");
  const choice = $("detail-status").value;
  try {
    await post(`${item.kind}/update`, { id: item.id, statusOverride: choice });
    await loadSubscriptions();
    const updated = currentItem();
    if (updated) openDetail(updated.id);
    setFeedback(
      node,
      choice === "auto"
        ? "Gespeichert. Der Status wird beim nächsten Prüflauf wieder selbst ermittelt."
        : "Gespeichert. Diese Angabe gilt, bis neue Kapitel auftauchen.",
      "ok"
    );
  } catch (error) {
    setFeedback(node, error.message, "error");
  }
});

/* Die Suche auf den Plattformen.
 *
 * Sie abonniert nichts: auf AniList und MyAnimeList stehen keine Kapitel, nur
 * Eintraege ueber Werke. Was sie loest, ist die Zuordnung — welcher Eintrag
 * gehoert zu meinem Download, wenn die Titel auseinandergehen. Ein Klick auf
 * einen Treffer traegt ihn als Statusquelle ein.
 *
 * NovelUpdates fehlt bewusst: keine API, dazu Cloudflare davor. Eine Suche,
 * die oft genug scheitert, ist schlechter als keine. NovelUpdates-Serien
 * abonniert man direkt ueber ihre Seiten-URL. */
async function runPlatformSearch() {
  const item = currentItem();
  if (!item) return;
  const node = $("detail-status-feedback");
  const hits = $("detail-search-hits");
  const query = $("detail-search-query").value.trim();
  if (!query) return;

  clear(hits);
  setFeedback(node, "Suche läuft …");
  try {
    const data = await api(
      `platform-search?title=${encodeURIComponent(query)}&kind=${item.kind}`
    );
    const results = data.results || [];
    if (results.length === 0) {
      setFeedback(node, "Nichts gefunden. Anderen Titel versuchen?", "error");
      return;
    }
    setFeedback(node, "");
    for (const hit of results) {
      hits.appendChild(searchHitRow(hit));
    }
  } catch (error) {
    setFeedback(node, error.message, "error");
  }
}

function searchHitRow(hit) {
  const row = el("div", "search-hit");
  if (hit.coverUrl) {
    const img = el("img");
    img.src = hit.coverUrl;
    img.loading = "lazy";
    img.alt = "";
    row.appendChild(img);
  }

  const text = el("div", "grow");
  text.appendChild(el("div", "card-title", hit.title));
  const parts = [];
  if (hit.status) parts.push(hit.status.toLowerCase().replace(/_/g, " "));
  if (hit.malUrl) parts.push("auch auf MyAnimeList");
  text.appendChild(el("div", "card-meta", parts.join(" · ")));
  row.appendChild(text);

  /* Zwei Knoepfe statt einer Auswahl: welche der beiden Plattformen als
   * Statusquelle dient, ist Geschmackssache — beide beantworten dieselbe
   * Frage, und der Eintrag ist derselbe. */
  const buttons = el("div", "row");
  for (const [label, url] of [
    ["AniList übernehmen", hit.anilistUrl],
    ["MAL übernehmen", hit.malUrl],
  ]) {
    if (!url) continue;
    const button = el("button", "action", label);
    button.type = "button";
    button.addEventListener("click", () => {
      $("detail-status-url").value = url;
      clear($("detail-search-hits"));
      setFeedback(
        $("detail-status-feedback"),
        "Übernommen — noch auf Speichern klicken.",
        "ok"
      );
    });
    buttons.appendChild(button);
  }
  row.appendChild(buttons);
  return row;
}

$("detail-search-go").addEventListener("click", runPlatformSearch);
$("detail-search-query").addEventListener("keydown", (event) => {
  if (event.key === "Enter") runPlatformSearch();
});

$("detail-status-url-save").addEventListener("click", async () => {
  const item = currentItem();
  if (!item) return;
  const node = $("detail-status-feedback");
  try {
    await post(`${item.kind}/update`, {
      id: item.id,
      statusSourceUrl: $("detail-status-url").value.trim(),
    });
    await loadSubscriptions();
    setFeedback(
      node,
      "Gespeichert. Der Status wird beim nächsten Prüflauf gelesen.",
      "ok"
    );
  } catch (error) {
    setFeedback(node, error.message, "error");
  }
});

/* Kategorie und Kapitel-Limit. Die Kategorie-Auswahl bleibt in der Engine des
 * Abos — aus einem Manga wird kein Webnovel, nur das Regal wechselt. */
function fillKindSelect(item) {
  const select = $("detail-kind");
  clear(select);
  for (const kind of mediaKinds) {
    if (kind.subscribable && engineFor(kind.id) === item.kind) {
      select.appendChild(new Option(kind.label, kind.id));
    }
  }
  select.value = item.mediaKind;
  $("detail-limit").value = item.downloadLimit || "";
}

$("detail-kind-save").addEventListener("click", async () => {
  const item = currentItem();
  if (!item) return;
  const node = $("detail-kind-feedback");
  try {
    await post(`${item.kind}/update`, {
      id: item.id,
      mediaKind: $("detail-kind").value,
      downloadLimit: Number($("detail-limit").value) || 0,
    });
    await loadSubscriptions();
    const updated = currentItem();
    if (updated) openDetail(updated.id);
    setFeedback(node, "Gespeichert.", "ok");
  } catch (error) {
    setFeedback(node, error.message, "error");
  }
});

$("detail-relocate").addEventListener("click", async () => {
  const item = currentItem();
  if (!item) return;
  const node = $("detail-target-feedback");
  const ok = await confirmAction(
    `Dateien von „${item.title}" jetzt zum Ziel verschieben?\n\n` +
      `Von: ${item.deliveredTo}\n` +
      "Das kann bei vielen Kapiteln einen Moment dauern.",
    "Verschieben"
  );
  if (!ok) return;
  try {
    setFeedback(node, "Wird verschoben …");
    await post("relocate", { id: item.id, kind: item.mediaKind });
    await loadSubscriptions();
    const updated = currentItem();
    if (updated) openDetail(updated.id);
    setFeedback(node, "Verschoben — die Dateien liegen jetzt am Ziel.", "ok");
  } catch (error) {
    setFeedback(node, error.message, "error");
  }
});

$("detail-pick-target").addEventListener("click", async () => {
  const item = currentItem();
  if (!item) return;
  try {
    const path = await pickFolder();
    if (!path) return;
    await post(`${item.kind}/update`, { id: item.id, targetDir: path });
    await loadSubscriptions();
    const updated = currentItem();
    if (updated) renderDetailTarget(updated);
    setFeedback($("detail-target-feedback"), "Ziel gespeichert.", "ok");
  } catch (error) {
    setFeedback($("detail-target-feedback"), error.message, "error");
  }
});

$("detail-clear-target").addEventListener("click", async () => {
  const item = currentItem();
  if (!item) return;
  try {
    await post(`${item.kind}/update`, { id: item.id, clearTargetDir: true });
    await loadSubscriptions();
    const updated = currentItem();
    if (updated) renderDetailTarget(updated);
    setFeedback($("detail-target-feedback"), "Wieder auf Standard gesetzt.", "ok");
  } catch (error) {
    setFeedback($("detail-target-feedback"), error.message, "error");
  }
});

/* Login- und Cloudflare-Fenster.
 *
 * Beide laufen in einem sichtbaren Fremdfenster; das Backend hält den Zustand
 * pro Host. Nach dem Öffnen wird gepollt, bis er terminal ist — ein Wechsel
 * passiert erst, wenn der Nutzer im Fenster fertig ist. */

function hostOf(url) {
  try {
    return new URL(url).host;
  } catch (error) {
    return "";
  }
}

async function refreshLoginState() {
  const item = currentItem();
  const node = $("detail-login-state");
  if (!item) return;
  const host = hostOf(item.url);
  try {
    const data = await api(`webnovel/login-status?host=${encodeURIComponent(host)}`);
    node.textContent = data.loggedIn
      ? `Angemeldet bei ${host}.`
      : `Keine gespeicherte Sitzung für ${host}.`;
    $("detail-logout").disabled = !data.loggedIn;
  } catch (error) {
    node.textContent = "";
  }
}

/** Pollt einen Host-Zustand, bis er nicht mehr `pending` ist. */
function pollHostState(path, host, node, done) {
  const timer = setInterval(async () => {
    try {
      const data = await api(`${path}?host=${encodeURIComponent(host)}`);
      if (data.state === "pending" || data.state === "running") return;
      clearInterval(timer);
      done(data);
    } catch (error) {
      clearInterval(timer);
      setFeedback(node, error.message, "error");
    }
  }, 1200);
}

$("detail-login").addEventListener("click", async () => {
  const item = currentItem();
  if (!item) return;
  const node = $("detail-login-feedback");
  const host = hostOf(item.url);
  try {
    setFeedback(node, "Fenster wird geöffnet …");
    await post("webnovel/login", { host });
    setFeedback(node, "Bitte im geöffneten Fenster anmelden.");
    pollHostState("webnovel/login-status", host, node, (data) => {
      setFeedback(
        node,
        data.loggedIn ? "Anmeldung gespeichert." : data.message || "Nicht angemeldet.",
        data.loggedIn ? "ok" : "error"
      );
      refreshLoginState();
    });
  } catch (error) {
    setFeedback(node, error.message, "error");
  }
});

$("detail-solve").addEventListener("click", async () => {
  const item = currentItem();
  if (!item) return;
  const node = $("detail-login-feedback");
  try {
    setFeedback(node, "Fenster wird geöffnet …");
    const result = await post("webnovel/solve", { url: item.url });
    const host = result.host || hostOf(item.url);
    setFeedback(node, "Bitte die Prüfung im geöffneten Fenster abschliessen.");
    pollHostState("webnovel/solve-status", host, node, (data) => {
      setFeedback(
        node,
        data.state === "solved" ? "Prüfung bestanden." : data.message || "Nicht bestanden.",
        data.state === "solved" ? "ok" : "error"
      );
    });
  } catch (error) {
    setFeedback(node, error.message, "error");
  }
});

$("detail-logout").addEventListener("click", async () => {
  const item = currentItem();
  if (!item) return;
  const node = $("detail-login-feedback");
  try {
    await post("webnovel/logout", { host: hostOf(item.url) });
    setFeedback(node, "Sitzung entfernt.", "ok");
    refreshLoginState();
  } catch (error) {
    setFeedback(node, error.message, "error");
  }
});

$("detail-check").addEventListener("click", async () => {
  const item = currentItem();
  if (!item) return;
  await startCheck(item.kind, item.id);
});

/* Quelle und Plattformen im Systembrowser. Die Knoepfe stehen nur da, wenn es
 * die Seite auch gibt — ein Knopf, der „nicht verlinkt" meldet, ist Ballast. */
function openExternal(url) {
  return api(`open-url?url=${encodeURIComponent(url)}`).catch((error) => {
    setFeedback($("detail-feedback"), error.message, "error");
  });
}

$("detail-source").addEventListener("click", () => {
  const item = currentItem();
  if (item) openExternal(item.url);
});

$("detail-anilist").addEventListener("click", () => {
  const item = currentItem();
  if (item && item.anilistUrl) openExternal(item.anilistUrl);
});

$("detail-mal").addEventListener("click", () => {
  const item = currentItem();
  if (item && item.malUrl) openExternal(item.malUrl);
});

/* Downloads fuer dieses eine Abo aussetzen. Es bleibt vollstaendig erhalten —
 * nur die Sammellaeufe gehen daran vorbei, bis der Haken wieder weg ist. */
$("detail-pause").addEventListener("change", async () => {
  const item = currentItem();
  if (!item) return;
  const paused = $("detail-pause").checked;
  try {
    await post(`${item.kind}/update`, { id: item.id, enabled: !paused });
    await loadSubscriptions();
    setFeedback(
      $("detail-feedback"),
      paused
        ? `Pausiert. Dieses Abo wird bei „Alle prüfen" übersprungen.`
        : "Wieder aktiv.",
      "ok"
    );
  } catch (error) {
    $("detail-pause").checked = !paused;
    setFeedback($("detail-feedback"), error.message, "error");
  }
});

$("detail-open").addEventListener("click", async () => {
  const item = currentItem();
  if (!item) return;
  try {
    await post("reveal", { id: item.id, kind: item.mediaKind });
  } catch (error) {
    setFeedback($("detail-feedback"), error.message, "error");
  }
});

/* Bestands-Abos auf das Blockschema umstellen. Ausdruecklich nur auf Klick:
 * dabei verschwinden die alten Dateien, und ein Lesestand darin ist weg. */
$("detail-rebuild").addEventListener("click", async () => {
  const item = currentItem();
  if (!item) return;
  const ok = await confirmAction(
    `Dateien von „${item.title}" neu aufteilen?\n\n` +
      "Die Kapitel werden in feste Blöcke geschrieben, die sich danach nie " +
      "wieder ändern. Die bisherigen Dateien dieses Abos werden dabei " +
      "entfernt — ein Lesestand darin geht verloren.\n\n" +
      "Nur Dateien, die Fero selbst geschrieben hat, sind betroffen.",
    "Neu aufteilen"
  );
  if (!ok) return;
  try {
    setFeedback($("detail-feedback"), "Wird neu aufgeteilt …");
    const result = await post("webnovel/rebuild-blocks", { id: item.id });
    setFeedback(
      $("detail-feedback"),
      `${result.written} Block-Dateien geschrieben, ${result.removed} alte entfernt.`,
      "ok"
    );
  } catch (error) {
    setFeedback($("detail-feedback"), error.message, "error");
  }
});

$("detail-delete").addEventListener("click", async () => {
  const item = currentItem();
  if (!item) return;
  const ok = await confirmAction(
    `„${item.title}" löschen?\n\nDas Abo wandert in den Papierkorb; die ` +
      "heruntergeladenen Dateien bleiben am Zielort liegen.",
    "Abo löschen"
  );
  if (!ok) return;
  try {
    await post(`${item.kind}/unsubscribe`, { id: item.id, keepFiles: true });
    status(`„${item.title}" entfernt. Die Dateien liegen weiterhin am Zielort.`);
    currentDetailId = null;
    showView("subscriptions");
    await loadSubscriptions();
  } catch (error) {
    setFeedback($("detail-feedback"), error.message, "error");
  }
});

// ── Prüflauf ─────────────────────────────────────────────────────────────

$("check-all").addEventListener("click", () => startCheck(null, null));

/* Der gerade laufende Job, damit der Stopp-Knopf weiss, wen er meint. */
let activeJob = null;

const ENGINE_LABELS = { webnovel: "Webnovels", manga: "Manga" };

/* Die beiden Engines nacheinander, nicht nur die erste.
 *
 * Vorher wurde nach dem ersten Job mit `return` abgebrochen — und weil die
 * Webnovel-Pruefung immer eine Job-Id liefert, kam die Manga-Pruefung bei
 * „Alle pruefen" nie an die Reihe. Wer nur Manga abonniert hatte, sah
 * „Keine passenden Abonnements." und keinen einzigen Download. */
async function startCheck(kind, id) {
  const kinds = kind ? [kind] : ["webnovel", "manga"];
  const messages = [];
  try {
    for (const each of kinds) {
      const result = await post(`${each}/check`, id ? { id } : {});
      if (!result.jobId) continue;
      const outcome = await runJob(each, result.jobId);
      if (outcome.message) {
        messages.push(
          kinds.length > 1
            ? `${ENGINE_LABELS[each]}: ${outcome.message}`
            : outcome.message
        );
      }
      // Wer stoppt, meint den ganzen Lauf — nicht nur die halbe Haelfte davon.
      if (outcome.stopped) break;
    }
    status(messages.join(" · ") || "Nichts zu tun — alle Abos sind aktuell.");
  } catch (error) {
    status(error.message, true);
  }
  await loadSubscriptions();
  if (currentDetailId) openDetail(currentDetailId);
}

/* Verfolgt einen Job bis zum Ende und loest mit seinem Schlusswort auf. */
function runJob(kind, jobId) {
  return new Promise((resolve) => {
    clearInterval(jobTimer);
    activeJob = { kind, jobId };
    const banner = $("job-banner");
    const stop = $("job-stop");
    banner.hidden = false;
    stop.disabled = false;
    stop.textContent = "Stoppen";

    const finish = (outcome) => {
      clearInterval(jobTimer);
      activeJob = null;
      banner.hidden = true;
      resolve(outcome);
    };

    const tick = async () => {
      let data;
      try {
        data = await api(`${kind}/job?job_id=${encodeURIComponent(jobId)}`);
      } catch (error) {
        status(error.message, true);
        finish({ message: null, stopped: true });
        return;
      }

      const job = data.status;
      if (!job) return;

      $("job-title").textContent =
        job.novelTitle || job.mangaTitle || job.seriesTitle || "Prüfe …";
      $("job-detail").textContent = job.totalChapters
        ? `Kapitel ${job.currentChapter} von ${job.totalChapters} · ${job.downloaded} geladen`
        : `${job.downloaded} geladen`;
      const share = job.totalChapters
        ? Math.round((job.currentChapter / job.totalChapters) * 100)
        : 0;
      $("job-bar-fill").style.width = `${Math.min(share, 100)}%`;
      // Zwischen zwei Kapiteln koennen Dutzende Bildabrufe liegen; ohne diese
      // Rueckmeldung sieht der Klick aus, als waere nichts passiert.
      if (job.cancelRequested) {
        stop.disabled = true;
        stop.textContent = "wird beendet …";
      }

      if (job.state !== "running") {
        if (job.state === "failed") status(job.message || "Lauf fehlgeschlagen.", true);
        finish({
          message: job.state === "failed" ? null : job.message,
          stopped: Boolean(job.cancelRequested),
        });
      }
    };

    tick();
    jobTimer = setInterval(tick, 900);
  });
}

$("job-stop").addEventListener("click", async () => {
  if (!activeJob) return;
  const stop = $("job-stop");
  stop.disabled = true;
  stop.textContent = "wird beendet …";
  try {
    await post(`${activeJob.kind}/stop`, { jobId: activeJob.jobId });
  } catch (error) {
    status(error.message, true);
    stop.disabled = false;
    stop.textContent = "Stoppen";
  }
});

// ── Papierkorb ───────────────────────────────────────────────────────────

async function loadTrash() {
  const [novels, mangas] = await Promise.all([
    api("webnovel/trash").catch(() => ({ entries: [] })),
    api("manga/trash").catch(() => ({ subscriptions: [] })),
  ]);

  /* Die beiden Routen antworten unterschiedlich: Webnovels liefern schlanke
   * Papierkorb-Eintraege, Manga die vollen Zusammenfassungen. Hier auf eine
   * gemeinsame Form bringen, statt das in der Darstellung zu verzweigen. */
  const entries = [
    ...(novels.entries || []).map((entry) => ({
      id: entry.id,
      title: entry.title,
      kind: "webnovel",
      filesInTrash: entry.filesInTrash,
      trashedAtUnix: entry.trashedAtUnix,
    })),
    ...(mangas.subscriptions || []).map((item) => ({
      id: item.id,
      title: item.title,
      kind: "manga",
      filesInTrash: false,
    })),
  ];

  $("nav-count-trash").textContent = entries.length || "";

  const list = $("trash-list");
  clear(list);
  $("trash-empty").hidden = entries.length > 0;

  for (const entry of entries) {
    const row = el("div", "card");
    const left = el("div");
    left.appendChild(el("div", "card-title", entry.title));
    const parts = [kindLabel(entry.kind)];
    if (entry.trashedAtUnix) {
      parts.push(new Date(entry.trashedAtUnix * 1000).toLocaleDateString("de-DE"));
    }
    if (entry.filesInTrash) parts.push("Dateien beiseitegelegt");
    left.appendChild(el("div", "card-meta", parts.join(" · ")));

    const actions = el("div", "row");
    const restore = el("button", "action", "Wiederherstellen");
    restore.addEventListener("click", () => trashAction(entry, "restore"));
    const purge = el("button", "action danger", "Endgültig löschen");
    purge.addEventListener("click", async () => {
      const ok = await confirmAction(
        `„${entry.title}" endgültig löschen? Das lässt sich nicht rückgängig machen.`,
        "Endgültig löschen"
      );
      if (ok) trashAction(entry, "purge");
    });
    actions.appendChild(restore);
    actions.appendChild(purge);

    row.appendChild(left);
    row.appendChild(actions);
    list.appendChild(row);
  }
}

async function trashAction(entry, action) {
  try {
    await post(`${entry.kind}/${action}`, { id: entry.id });
    await loadTrash();
    await loadSubscriptions();
    setFeedback(
      $("trash-feedback"),
      action === "restore" ? "Wiederhergestellt." : "Endgültig gelöscht.",
      "ok"
    );
  } catch (error) {
    setFeedback($("trash-feedback"), error.message, "error");
  }
}

// ── Einstellungen ────────────────────────────────────────────────────────

async function loadTargets() {
  let data;
  try {
    data = await api("targets");
  } catch (error) {
    setFeedback($("settings-feedback"), error.message, "error");
    return;
  }

  dataDir = data.dataDir || null;
  $("data-dir").textContent = data.dataDir || "noch nicht eingerichtet";
  setFeedback($("data-dir-problem"), data.dataDirProblem || "", "error");
  if (data.dataDir) {
    $("data-dir").textContent =
      data.dataDir + (data.portable ? "  (portabel, neben der App)" : "");
  }

  mediaKinds = data.kinds || [];
  syncKindSelectors();
  renderTargetRows(data);
  updateTargetHint(data);
  setFeedback($("settings-feedback"), "");
}

/** Hält die Typ-Auswahlfelder mit dem Backend in Deckung. */
function syncKindSelectors() {
  const addSelect = $("add-kind");
  const filterSelect = $("kind-filter");
  const previousFilter = filterSelect.value;

  clear(addSelect);
  clear(filterSelect);
  filterSelect.appendChild(new Option("Alle Typen", ""));
  for (const kind of mediaKinds) {
    if (kind.subscribable) {
      addSelect.appendChild(new Option(kind.label, kind.id));
    }
    filterSelect.appendChild(new Option(kind.label, kind.id));
  }
  filterSelect.value = previousFilter;
}

function renderTargetRows(data) {
  const container = $("target-rows");
  clear(container);

  for (const kind of data.kinds) {
    const row = el("div", "target-row");
    row.appendChild(el("span", null, kind.label));

    const path = el("span", "mono" + (kind.directory ? "" : " unset"));
    path.textContent = kind.directory || "nicht gesetzt";
    row.appendChild(path);

    const pick = el("button", "action", "Wählen");
    pick.addEventListener("click", () => saveTarget(kind.id, true));
    row.appendChild(pick);

    const clearButton = el("button", "action ghost", "Entfernen");
    clearButton.disabled = !kind.directory;
    clearButton.addEventListener("click", () => saveTarget(kind.id, false));
    row.appendChild(clearButton);

    container.appendChild(row);
  }

  const fallback = $("fallback-path");
  fallback.textContent = data.fallback || "nicht gesetzt";
  fallback.className = "mono grow" + (data.fallback ? "" : " unset");
}

/* Datenordner frei waehlen — der Ausweg, wenn der Ordner neben der App nicht
 * beschreibbar ist (z.B. weil macOS die App transloziert hat). */
$("data-dir-pick").addEventListener("click", async () => {
  try {
    const directory = await pickFolder();
    if (!directory) return;
    await post("data-dir/save", { directory });
    await loadTargets();
    setFeedback(
      $("data-dir-feedback"),
      "Datenordner gesetzt. Abos und Einstellungen liegen jetzt dort.",
      "ok"
    );
  } catch (error) {
    setFeedback($("data-dir-feedback"), error.message, "error");
  }
});

document.querySelectorAll("[data-role='pick'], [data-role='clear']").forEach((button) => {
  button.addEventListener("click", () => {
    saveTarget(button.dataset.targetKind || null, button.dataset.role === "pick");
  });
});

async function saveTarget(kind, choose) {
  try {
    let directory = null;
    if (choose) {
      directory = await pickFolder();
      if (!directory) return;
    }
    await post("targets/save", { kind: kind || null, directory });
    await loadTargets();
    setFeedback($("settings-feedback"), "Gespeichert.", "ok");
  } catch (error) {
    setFeedback($("settings-feedback"), error.message, "error");
  }
}

/** Weist in der Seitenleiste darauf hin, wenn noch kein Ziel eingerichtet ist. */
function updateTargetHint(data) {
  const configured =
    (data.kinds || []).some((kind) => kind.directory) || Boolean(data.fallback);
  $("target-hint").textContent = configured
    ? ""
    : "Noch kein Zielordner festgelegt — siehe Einstellungen.";
}

// ── Übernahme ────────────────────────────────────────────────────────────

$("import-scan").addEventListener("click", async () => {
  const node = $("import-feedback");
  const button = $("import-scan");
  button.disabled = true;
  setFeedback(node, "Durchsuche Zielordner …");
  try {
    const result = await post("import", {});
    const parts = [];
    if (result.imported.length) {
      parts.push(`${result.imported.length} übernommen: ${result.imported.join(", ")}`);
    }
    if (result.skipped) parts.push(`${result.skipped} bereits vorhanden`);
    if (result.unmatchedChapters) {
      parts.push(
        `${result.unmatchedChapters} Kapitel ohne Wiedererkennung — sie werden beim nächsten Prüflauf neu geladen`
      );
    }
    setFeedback(node, parts.length ? parts.join(" · ") : "Nichts gefunden.", "ok");
    await loadSubscriptions();
  } catch (error) {
    setFeedback(node, error.message, "error");
  } finally {
    button.disabled = false;
  }
});

// ── Zeitplan ─────────────────────────────────────────────────────────────

async function loadSchedule() {
  try {
    const data = await api("schedule");
    $("schedule-interval").value = data.intervalHours;
    $("schedule-paused").checked = data.paused;
    $("schedule-quit").checked = data.quitOnClose;
  } catch (error) {
    setFeedback($("schedule-feedback"), error.message, "error");
  }
}

async function saveSchedule() {
  try {
    const data = await post("schedule/save", {
      intervalHours: Number($("schedule-interval").value) || undefined,
      paused: $("schedule-paused").checked,
      quitOnClose: $("schedule-quit").checked,
    });
    /* Das Backend begrenzt das Intervall auf 1 Stunde bis 7 Tage; den
     * zurueckgegebenen Wert uebernehmen, damit das Feld nicht luegt. */
    $("schedule-interval").value = data.intervalHours;
    setFeedback($("schedule-feedback"), "Gespeichert.", "ok");
  } catch (error) {
    setFeedback($("schedule-feedback"), error.message, "error");
  }
}

$("schedule-save").addEventListener("click", saveSchedule);
$("schedule-paused").addEventListener("change", saveSchedule);
$("schedule-quit").addEventListener("change", saveSchedule);

// ── Blockliste ───────────────────────────────────────────────────────────

let blocklist = [];

async function loadBlocklist() {
  try {
    const data = await api("webnovel/blocklist");
    blocklist = data.entries || [];
  } catch (error) {
    setFeedback($("blocklist-feedback"), error.message, "error");
    return;
  }

  const container = $("blocklist-rows");
  clear(container);
  for (const entry of blocklist) {
    const row = el("div", "target-row");
    row.appendChild(el("span", "mono", entry.host));
    row.appendChild(el("span", entry.note ? "" : "unset", entry.note || "—"));
    row.appendChild(el("span", "card-meta", entry.builtin ? "mitgeliefert" : ""));

    const remove = el("button", "action ghost", "Entfernen");
    remove.disabled = entry.builtin;
    remove.addEventListener("click", () => saveBlocklist(
      blocklist.filter((other) => other.host !== entry.host)
    ));
    row.appendChild(remove);
    container.appendChild(row);
  }
}

/* Das Backend nimmt die vollständige Liste entgegen, nicht einzelne Änderungen.
 * Mitgelieferte Einträge wandern mit — es filtert sie selbst wieder heraus. */
async function saveBlocklist(entries) {
  try {
    await post("webnovel/blocklist/save", { entries });
    await loadBlocklist();
    setFeedback($("blocklist-feedback"), "Gespeichert.", "ok");
  } catch (error) {
    setFeedback($("blocklist-feedback"), error.message, "error");
  }
}

$("block-add").addEventListener("click", () => {
  const host = $("block-host").value.trim().toLowerCase();
  if (!host) {
    setFeedback($("blocklist-feedback"), "Bitte einen Host angeben.", "error");
    return;
  }
  if (blocklist.some((entry) => entry.host === host)) {
    setFeedback($("blocklist-feedback"), "Dieser Host steht bereits auf der Liste.", "error");
    return;
  }
  const note = $("block-note").value.trim();
  $("block-host").value = "";
  $("block-note").value = "";
  saveBlocklist([...blocklist, { host, note: note || null, builtin: false }]);
});

// ── Protokoll ────────────────────────────────────────────────────────────

async function loadLog() {
  try {
    const data = await api("webnovel/debug-log");
    $("log-path").textContent = data.path || "";
    $("log-body").textContent = data.content || "(leer)";
  } catch (error) {
    $("log-body").textContent = error.message;
  }
}

$("log-reload").addEventListener("click", loadLog);
$("log-open").addEventListener("click", async () => {
  try {
    await api("webnovel/open-debug-log");
  } catch (error) {
    status(error.message, true);
  }
});

// ── Start ────────────────────────────────────────────────────────────────

(async function start() {
  $("view-list").classList.toggle("is-active", viewMode === "list");
  $("view-grid").classList.toggle("is-active", viewMode === "grid");
  // Zuerst die Ziele: sie liefern die Medientypen, aus denen sich die
  // Auswahlfelder aufbauen.
  await loadTargets();
  await loadSubscriptions();
})();
