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
  if (!response.ok) {
    throw new Error(`${response.status} ${response.statusText}`);
  }
  const data = await response.json();
  if (data && data.error) throw new Error(data.error);
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
    if (view === "settings") loadTargets();
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
    ...(novels.subscriptions || []).map((item) => ({ ...item, kind: "webnovel" })),
    ...(mangas.subscriptions || []).map((item) => ({ ...item, kind: "manga" })),
  ].sort((a, b) => a.title.localeCompare(b.title, "de"));

  $("nav-count-subscriptions").textContent = subscriptions.length || "";
  renderSubscriptions();
}

function kindLabel(id) {
  const kind = mediaKinds.find((entry) => entry.id === id);
  return kind ? kind.label : id;
}

function visibleSubscriptions() {
  const needle = $("filter-input").value.trim().toLowerCase();
  const kind = $("kind-filter").value;
  return subscriptions.filter((item) => {
    if (kind && item.kind !== kind) return false;
    if (!needle) return true;
    return (
      item.title.toLowerCase().includes(needle) ||
      item.source.toLowerCase().includes(needle)
    );
  });
}

function renderSubscriptions() {
  const list = $("subscription-list");
  const empty = $("subscription-empty");
  clear(list);

  const items = visibleSubscriptions();
  if (items.length === 0) {
    empty.hidden = false;
    empty.textContent = subscriptions.length
      ? "Keine Treffer für diesen Filter."
      : "Noch keine Abos. Oben rechts eins hinzufügen.";
    return;
  }
  empty.hidden = true;

  for (const item of items) {
    const card = el("button", "card");
    card.type = "button";

    const left = el("div");
    left.appendChild(el("div", "card-title", item.title));
    left.appendChild(
      el("div", "card-meta", `${kindLabel(item.kind)} · ${item.source}`)
    );

    const badges = el("div", "badges");
    if (!item.enabled) badges.appendChild(el("span", "badge off", "pausiert"));
    if (item.completed) badges.appendChild(el("span", "badge ok", "abgeschlossen"));
    if (item.hiatus) badges.appendChild(el("span", "badge warn", "Hiatus"));
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

$("filter-input").addEventListener("input", renderSubscriptions);
$("kind-filter").addEventListener("change", renderSubscriptions);

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
    const result = await post(`${kind}/subscribe`, { url });
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
  fact(facts, "Typ", kindLabel(item.kind));
  fact(facts, "Quelle", item.source);
  fact(facts, "Autor", item.author);
  fact(facts, "Kapitel", `${item.downloadedChapters} von ${item.knownChapters} geladen`);
  fact(facts, "Status", item.completed ? "abgeschlossen" : item.hiatus ? "Hiatus" : "laufend");
  if (item.lastCheckUnix) {
    fact(facts, "Letzter Lauf", new Date(item.lastCheckUnix * 1000).toLocaleString("de-DE"));
  }
  if (item.lastError) fact(facts, "Letzter Fehler", item.lastError);
  if (item.ratingExternal) fact(facts, "Bewertung", `${item.ratingExternal} / 5`);

  $("detail-description").textContent = item.description || "";

  const genres = $("detail-genres");
  clear(genres);
  for (const genre of item.genres || []) {
    genres.appendChild(el("span", "badge", genre));
  }

  renderDetailTarget(item);
  setFeedback($("detail-feedback"), "");
  setFeedback($("detail-target-feedback"), "");
  showView("detail");
}

function renderDetailTarget(item) {
  const node = $("detail-target-current");
  if (item.targetDir) {
    node.textContent = `Eigener Ordner: ${item.targetDir}`;
  } else {
    node.textContent = `Standard für ${kindLabel(item.kind)} wird verwendet.`;
  }
  $("detail-clear-target").disabled = !item.targetDir;
}

function currentItem() {
  return subscriptions.find((entry) => entry.id === currentDetailId);
}

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

$("detail-check").addEventListener("click", async () => {
  const item = currentItem();
  if (!item) return;
  await startCheck(item.kind, item.id);
});

$("detail-source").addEventListener("click", async () => {
  const item = currentItem();
  if (!item) return;
  try {
    await api(`open-url?url=${encodeURIComponent(item.url)}`);
  } catch (error) {
    setFeedback($("detail-feedback"), error.message, "error");
  }
});

$("detail-open").addEventListener("click", () => {
  setFeedback(
    $("detail-feedback"),
    "Ordner anzeigen folgt, sobald die Route auf Abo-Ids umgestellt ist.",
    "error"
  );
});

$("detail-delete").addEventListener("click", async () => {
  const item = currentItem();
  if (!item) return;
  const keep = window.confirm(
    `„${item.title}" löschen?\n\nOK: nur das Abo entfernen, die Dateien bleiben liegen.\nAbbrechen: nichts tun.`
  );
  if (!keep) return;
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

async function startCheck(kind, id) {
  const kinds = kind ? [kind] : ["webnovel", "manga"];
  try {
    for (const each of kinds) {
      const result = await post(`${each}/check`, id ? { id } : {});
      if (result.jobId) {
        pollJob(each, result.jobId);
        return;
      }
    }
    status("Nichts zu tun — alle Abos sind aktuell.");
    await loadSubscriptions();
  } catch (error) {
    status(error.message, true);
  }
}

function pollJob(kind, jobId) {
  clearInterval(jobTimer);
  const banner = $("job-banner");
  banner.hidden = false;

  const tick = async () => {
    let data;
    try {
      data = await api(`${kind}/job?job_id=${encodeURIComponent(jobId)}`);
    } catch (error) {
      clearInterval(jobTimer);
      banner.hidden = true;
      status(error.message, true);
      return;
    }

    const job = data.status;
    if (!job) return;

    $("job-title").textContent = job.novelTitle || job.seriesTitle || "Prüfe …";
    $("job-detail").textContent = job.totalChapters
      ? `Kapitel ${job.currentChapter} von ${job.totalChapters} · ${job.downloaded} geladen`
      : `${job.downloaded} geladen`;
    const share = job.totalChapters
      ? Math.round((job.currentChapter / job.totalChapters) * 100)
      : 0;
    $("job-bar-fill").style.width = `${Math.min(share, 100)}%`;

    if (job.state !== "running") {
      clearInterval(jobTimer);
      banner.hidden = true;
      status(job.message || "Lauf beendet.", job.state === "failed");
      await loadSubscriptions();
      if (currentDetailId) openDetail(currentDetailId);
    }
  };

  tick();
  jobTimer = setInterval(tick, 900);
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
    addSelect.appendChild(new Option(kind.label, kind.id));
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

// ── Protokoll ────────────────────────────────────────────────────────────

async function loadLog() {
  try {
    const data = await api("webnovel/debug-log");
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
  // Zuerst die Ziele: sie liefern die Medientypen, aus denen sich die
  // Auswahlfelder aufbauen.
  await loadTargets();
  await loadSubscriptions();
})();
