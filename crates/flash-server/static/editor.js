// The advanced card editor (external file: CSP forbids inline script).
//
// Progressive enhancement over card_editor_body.html: the contenteditable
// fields mirror their HTML into hidden inputs (front_html / back_html) on
// every change, so a plain form post — or the htmx dialog post — carries
// exactly what is on screen. Formatting emits only markup the server's
// sanitizer keeps: b/i/u/s/sup/sub, lists, and hl-{hue} / hl-bg-{hue}
// spans (never inline styles). Media is uploaded on paste or drop and
// inserted as <img>/<audio>/<video src="/media/{id}">.
(function () {
  "use strict";

  var MAX_UPLOAD = 100 * 1024 * 1024;
  var LABELS = {
    basic: ["Front", "Back", "Question", "Answer"],
    basic_reversed: ["Front", "Back", "Question", "Answer"],
    basic_typed: ["Front", "Back", "Question", "Answer to type"],
    cloze: ["Text", "Extra", "Sentence with {{c1::blanks}}", "Shown with the answer (optional)"],
  };

  function escapeHtml(s) {
    return String(s)
      .replace(/&/g, "&amp;")
      .replace(/</g, "&lt;")
      .replace(/>/g, "&gt;")
      .replace(/"/g, "&quot;");
  }
  function textToHtml(t) {
    return escapeHtml(t).replace(/\r?\n/g, "<br>");
  }

  // Pasted HTML: keep structure, drop every attribute (styles, classes,
  // handlers, external srcs). The server sanitizes again regardless.
  function cleanHtml(html) {
    var tpl = document.createElement("template");
    tpl.innerHTML = html;
    var walker = document.createTreeWalker(tpl.content, NodeFilter.SHOW_ELEMENT);
    var nodes = [];
    while (walker.nextNode()) nodes.push(walker.currentNode);
    nodes.forEach(function (el) {
      var tag = el.tagName.toLowerCase();
      if (tag === "script" || tag === "style" || tag === "meta" || tag === "link" || tag === "iframe") {
        el.remove();
        return;
      }
      var keepSrc = (tag === "img" || tag === "audio" || tag === "video") && /^\/media\/\d+$/.test(el.getAttribute("src") || "");
      if ((tag === "img" || tag === "audio" || tag === "video") && !keepSrc) {
        el.remove();
        return;
      }
      Array.prototype.slice.call(el.attributes).forEach(function (a) {
        if (keepSrc && (a.name === "src" || a.name === "controls")) return;
        el.removeAttribute(a.name);
      });
    });
    return tpl.innerHTML;
  }

  function caretFromPoint(x, y, within) {
    var range = null;
    if (document.caretRangeFromPoint) {
      range = document.caretRangeFromPoint(x, y);
    } else if (document.caretPositionFromPoint) {
      var pos = document.caretPositionFromPoint(x, y);
      if (pos) {
        range = document.createRange();
        range.setStart(pos.offsetNode, pos.offset);
        range.collapse(true);
      }
    }
    if (range && !within.contains(range.startContainer)) return null;
    return range;
  }

  function unwrap(el) {
    var parent = el.parentNode;
    while (el.firstChild) parent.insertBefore(el.firstChild, el);
    parent.removeChild(el);
  }

  function initEditor(root) {
    if (root.dataset.editorInit) return;
    root.dataset.editorInit = "1";
    var form = root.closest("form");
    var fields = Array.prototype.slice.call(root.querySelectorAll(".editor-field"));
    var typeSel = root.querySelector("[data-editor-type]");
    var status = root.querySelector("[data-editor-status]");
    var fileInput = root.querySelector("[data-editor-file]");
    var lastField = fields[0];

    try { document.execCommand("styleWithCSS", false, false); } catch (e) { /* older engines */ }

    function hiddenFor(field) {
      return root.querySelector('input[name="' + field.dataset.field + '_html"]');
    }
    function sourceFor(field) {
      return root.querySelector('[data-source-for="' + field.dataset.field + '"]');
    }
    function sync(field) {
      var src = sourceFor(field);
      if (src && !src.hidden) field.innerHTML = src.value;
      var hidden = hiddenFor(field);
      if (hidden) hidden.value = field.innerHTML;
    }
    function syncAll() { fields.forEach(sync); }
    function note(msg) {
      if (!status) return;
      status.textContent = msg || "";
      status.hidden = !msg;
      if (msg) setTimeout(function () { if (status.textContent === msg) { status.hidden = true; } }, 6000);
    }
    function fieldOf(node) {
      while (node && node !== root) {
        if (node.nodeType === 1 && node.classList.contains("editor-field")) return node;
        node = node.parentNode;
      }
      return null;
    }
    function activeField() {
      var sel = window.getSelection();
      var f = sel && sel.anchorNode ? fieldOf(sel.anchorNode) : null;
      return f || lastField;
    }
    function selectionIn(field) {
      var sel = window.getSelection();
      if (!sel || !sel.rangeCount) return null;
      var range = sel.getRangeAt(0);
      if (fieldOf(range.commonAncestorContainer) !== field) return null;
      return range;
    }
    function placeCaretAtEnd(field) {
      var r = document.createRange();
      r.selectNodeContents(field);
      r.collapse(false);
      var sel = window.getSelection();
      sel.removeAllRanges();
      sel.addRange(r);
    }

    // ---- type picker ----
    function applyType() {
      var t = typeSel ? typeSel.value : root.dataset.type;
      root.dataset.type = t;
      var l = LABELS[t] || LABELS.basic;
      var fl = root.querySelector("[data-front-label]");
      var bl = root.querySelector("[data-back-label]");
      if (fl) fl.textContent = l[0];
      if (bl) bl.textContent = l[1];
      if (fields[0]) fields[0].dataset.placeholder = l[2];
      if (fields[1]) fields[1].dataset.placeholder = l[3];
    }
    if (typeSel) typeSel.addEventListener("change", applyType);
    applyType();

    // ---- fields ----
    fields.forEach(function (field) {
      field.addEventListener("input", function () { sync(field); });
      field.addEventListener("focus", function () { lastField = field; });
      field.addEventListener("paste", function (e) {
        var cd = e.clipboardData;
        if (!cd) return;
        if (cd.files && cd.files.length) {
          e.preventDefault();
          Array.prototype.forEach.call(cd.files, function (f) { upload(field, f, null); });
          return;
        }
        var html = cd.getData("text/html");
        var text = cd.getData("text/plain");
        if (html) {
          e.preventDefault();
          document.execCommand("insertHTML", false, cleanHtml(html));
        } else if (text) {
          e.preventDefault();
          document.execCommand("insertText", false, text);
        }
        sync(field);
      });
      var depth = 0;
      field.addEventListener("dragenter", function (e) {
        e.preventDefault();
        depth++;
        field.classList.add("is-drag");
      });
      field.addEventListener("dragover", function (e) { e.preventDefault(); });
      field.addEventListener("dragleave", function () {
        depth = Math.max(0, depth - 1);
        if (depth === 0) field.classList.remove("is-drag");
      });
      field.addEventListener("drop", function (e) {
        depth = 0;
        field.classList.remove("is-drag");
        var files = e.dataTransfer && e.dataTransfer.files;
        if (!files || !files.length) return;
        e.preventDefault();
        var range = caretFromPoint(e.clientX, e.clientY, field);
        Array.prototype.forEach.call(files, function (f) { upload(field, f, range); });
      });
      field.addEventListener("keydown", function (e) {
        var mod = e.ctrlKey || e.metaKey;
        if (mod && e.shiftKey && (e.key === "C" || e.key === "c")) {
          e.preventDefault();
          insertCloze(field, e.altKey);
        } else if (mod && e.key === "Enter") {
          e.preventDefault();
          if (form) {
            if (form.requestSubmit) form.requestSubmit();
            else form.submit();
          }
        }
      });
    });
    root.querySelectorAll(".editor-source").forEach(function (src) {
      src.addEventListener("input", function () {
        var field = root.querySelector('.editor-field[data-field="' + src.dataset.sourceFor + '"]');
        if (field) sync(field);
      });
    });

    // ---- media ----
    function upload(field, file, range) {
      if (!file) return;
      if (file.size > MAX_UPLOAD) { note(file.name + " is larger than 100 MB."); return; }
      field.focus();
      lastField = field;
      if (range) {
        var sel = window.getSelection();
        sel.removeAllRanges();
        sel.addRange(range);
      } else if (!selectionIn(field)) {
        placeCaretAtEnd(field);
      } else {
        // Never swallow a highlighted selection: attach after it.
        window.getSelection().collapseToEnd();
      }
      var id = "up" + Date.now().toString(36) + Math.random().toString(36).slice(2, 7);
      document.execCommand(
        "insertHTML",
        false,
        '<span class="editor-uploading" id="' + id + '" contenteditable="false">Uploading ' + escapeHtml(file.name) + "…</span>&nbsp;"
      );
      sync(field);
      var fd = new FormData();
      fd.append("file", file, file.name);
      fetch("/media", { method: "POST", body: fd, credentials: "same-origin" })
        .then(function (r) {
          return r.json().catch(function () { return {}; }).then(function (j) { return { ok: r.ok, status: r.status, j: j }; });
        })
        .then(function (res) {
          var ph = document.getElementById(id);
          if (!res.ok) throw new Error(res.j.error || ("upload failed (" + res.status + ")"));
          var el;
          if (res.j.kind === "image") {
            el = document.createElement("img");
            el.src = "/media/" + res.j.id;
            el.alt = "";
          } else {
            el = document.createElement(res.j.kind === "video" ? "video" : "audio");
            el.controls = true;
            el.preload = "none";
            el.src = "/media/" + res.j.id;
          }
          if (ph) ph.replaceWith(el); else field.appendChild(el);
          sync(field);
        })
        .catch(function (err) {
          var ph = document.getElementById(id);
          if (ph) ph.remove();
          sync(field);
          note(err.message || "Upload failed.");
        });
    }
    var attach = root.querySelector("[data-attach]");
    if (attach && fileInput) {
      attach.addEventListener("mousedown", function (e) { e.preventDefault(); });
      attach.addEventListener("click", function () { fileInput.click(); });
      fileInput.addEventListener("change", function () {
        var field = activeField();
        Array.prototype.forEach.call(fileInput.files, function (f) { upload(field, f, null); });
        fileInput.value = "";
      });
    }

    // ---- formatting ----
    function wrapSelection(tag, cls, prefix) {
      var field = activeField();
      var range = selectionIn(field);
      if (!range || range.collapsed) { note("Select some text first."); return; }
      // Toggle/swap when the whole selection already sits inside a match.
      var node = range.commonAncestorContainer;
      while (node && node !== field) {
        if (node.nodeType === 1 && node.tagName.toLowerCase() === tag) {
          var has = prefix
            ? Array.prototype.some.call(node.classList, function (c) { return c.indexOf(prefix) === 0 && (prefix !== "hl-" || c.indexOf("hl-bg-") !== 0); })
            : true;
          if (has) {
            if (cls && node.className !== cls) node.className = cls; else unwrap(node);
            sync(field);
            return;
          }
        }
        node = node.parentNode;
      }
      if (!cls && prefix) { clearWithin(field, range, 'span[class^="' + prefix + '"]', prefix); sync(field); return; }
      var el = document.createElement(tag);
      if (cls) el.className = cls;
      try {
        range.surroundContents(el);
      } catch (e) {
        var frag = range.extractContents();
        el.appendChild(frag);
        range.insertNode(el);
      }
      var sel = window.getSelection();
      var r = document.createRange();
      r.selectNodeContents(el);
      sel.removeAllRanges();
      sel.addRange(r);
      sync(field);
    }
    function clearWithin(field, range, selector, prefix) {
      Array.prototype.slice.call(field.querySelectorAll(selector)).forEach(function (el) {
        if (!range.intersectsNode(el)) return;
        if (prefix === "hl-") {
          // Text-color clear must leave highlight spans alone and vice versa.
          var isBg = el.className.indexOf("hl-bg-") === 0;
          if (selector.indexOf("hl-bg-") >= 0 ? !isBg : isBg) return;
        }
        unwrap(el);
      });
    }
    function clearFormatting(field) {
      var range = selectionIn(field);
      if (!range || range.collapsed) { note("Select some text first."); return; }
      document.execCommand("removeFormat", false, null);
      range = selectionIn(field) || range;
      Array.prototype.slice.call(field.querySelectorAll('span[class^="hl-"], s, sup, sub, b, i, u, strong, em')).forEach(function (el) {
        if (range.intersectsNode(el)) unwrap(el);
      });
    }
    root.querySelectorAll("[data-cmd]").forEach(function (btn) {
      btn.addEventListener("mousedown", function (e) { e.preventDefault(); });
      btn.addEventListener("click", function () {
        var field = activeField();
        if (!field) return;
        field.focus();
        var cmd = btn.dataset.cmd;
        if (cmd === "strike") wrapSelection("s", "", "");
        else if (cmd === "clear") clearFormatting(field);
        else document.execCommand(cmd, false, null);
        sync(field);
      });
    });
    root.querySelectorAll(".editor-hues").forEach(function (details) {
      var summary = details.querySelector("summary");
      summary.addEventListener("mousedown", function (e) { e.preventDefault(); });
      details.querySelectorAll(".editor-swatch").forEach(function (sw) {
        sw.addEventListener("mousedown", function (e) { e.preventDefault(); });
        sw.addEventListener("click", function () {
          var bg = sw.hasAttribute("data-hl-bg");
          var hue = bg ? sw.dataset.hlBg : sw.dataset.hl;
          var prefix = bg ? "hl-bg-" : "hl-";
          wrapSelection("span", hue ? prefix + hue : "", prefix);
          details.open = false;
        });
      });
    });
    document.addEventListener("click", function (e) {
      root.querySelectorAll(".editor-hues[open]").forEach(function (d) {
        if (!d.contains(e.target)) d.open = false;
      });
    });

    // ---- cloze ----
    function insertCloze(field, reuse) {
      if (root.dataset.type !== "cloze" && typeSel) {
        typeSel.value = "cloze";
        applyType();
      }
      var range = selectionIn(field);
      if (!range) { field.focus(); placeCaretAtEnd(field); range = selectionIn(field); }
      if (!range) return;
      var max = 0, m, re = /\{\{c(\d+)::/g;
      while ((m = re.exec(field.innerHTML))) max = Math.max(max, parseInt(m[1], 10));
      var n = reuse ? Math.max(1, max) : max + 1;
      var frag = range.extractContents();
      // Word selections (double-click, Ctrl+Shift+Arrow) drag the trailing
      // space along; keep it outside the blank so the sentence reads on.
      var trailing = "";
      var last = frag.lastChild;
      while (last && last.lastChild) last = last.lastChild;
      if (last && last.nodeType === 3) {
        var m2 = /\s+$/.exec(last.data);
        if (m2 && m2[0].length < last.data.length) {
          trailing = m2[0];
          last.data = last.data.slice(0, -trailing.length);
        }
      }
      var wrap = document.createDocumentFragment();
      wrap.appendChild(document.createTextNode("{{c" + n + "::"));
      var empty = !frag.textContent;
      wrap.appendChild(frag);
      var close = document.createTextNode("}}" + trailing);
      wrap.appendChild(close);
      range.insertNode(wrap);
      var sel = window.getSelection();
      var r = document.createRange();
      if (empty) r.setStart(close, 0); else r.setStartAfter(close);
      r.collapse(true);
      sel.removeAllRanges();
      sel.addRange(r);
      sync(field);
    }
    var clozeBtn = root.querySelector("[data-cloze]");
    if (clozeBtn) {
      clozeBtn.addEventListener("mousedown", function (e) { e.preventDefault(); });
      clozeBtn.addEventListener("click", function () { insertCloze(activeField(), false); });
    }

    // ---- HTML source view ----
    var srcBtn = root.querySelector("[data-source]");
    if (srcBtn) {
      srcBtn.addEventListener("click", function () {
        var on = !srcBtn.classList.contains("is-active");
        fields.forEach(function (field) {
          var src = sourceFor(field);
          if (!src) return;
          if (on) {
            src.value = field.innerHTML;
            src.hidden = false;
            field.hidden = true;
          } else {
            field.innerHTML = src.value;
            src.hidden = true;
            field.hidden = false;
          }
          sync(field);
        });
        srcBtn.classList.toggle("is-active", on);
      });
    }

    // ---- submit ----
    if (form) {
      form.addEventListener("submit", function (e) {
        syncAll();
        if (root.closest("[hidden]")) return; // basic mode: the editor is dormant
        if (root.querySelector(".editor-uploading")) {
          e.preventDefault();
          e.stopImmediatePropagation();
          note("Wait for the upload to finish.");
        }
      }, true);
    }
    root.flashSync = syncAll;
  }

  // Deck page: the Advanced toggle turns the quick form into the editor.
  function initToggle(form) {
    var btn = form.querySelector("[data-editor-toggle]");
    var quick = form.querySelector("[data-quick-fields]");
    var adv = form.querySelector(".editor-advanced");
    if (!btn || !quick || !adv || form.dataset.toggleInit) return;
    form.dataset.toggleInit = "1";
    var textareas = quick.querySelectorAll("textarea");
    var fields = adv.querySelectorAll(".editor-field");
    function set(on, focus) {
      adv.hidden = !on;
      quick.hidden = on;
      btn.setAttribute("aria-expanded", on ? "true" : "false");
      btn.textContent = on ? "Simple" : "Advanced";
      form.action = on ? form.dataset.notesAction : form.dataset.basicAction;
      textareas.forEach(function (t) { t.required = !on; });
      if (on) {
        // Carry anything already typed into the rich fields, once.
        textareas.forEach(function (t, i) {
          var f = fields[i];
          if (f && !f.textContent.trim() && t.value.trim()) {
            f.innerHTML = textToHtml(t.value);
            f.dispatchEvent(new Event("input"));
          }
        });
        if (focus && fields[0]) fields[0].focus();
      } else {
        fields.forEach(function (f, i) {
          var t = textareas[i];
          if (t && !t.value.trim() && f.textContent.trim()) t.value = f.textContent;
        });
      }
      try {
        if (on) localStorage.setItem("flash.editor.advanced", "1");
        else localStorage.removeItem("flash.editor.advanced");
      } catch (e) { /* storage blocked */ }
    }
    btn.hidden = false;
    btn.addEventListener("click", function () { set(adv.hidden, true); });
    var remembered = false;
    try { remembered = localStorage.getItem("flash.editor.advanced") === "1"; } catch (e) { /* storage blocked */ }
    if (remembered) set(true, false);
  }

  // Edit dialog lifecycle.
  function closeDialog() {
    var host = document.getElementById("editor-dialog");
    if (host) host.innerHTML = "";
  }
  function initDialog(host) {
    host.querySelectorAll("[data-editor-close]").forEach(function (b) {
      b.addEventListener("click", closeDialog);
    });
    var overlay = host.querySelector("[data-editor-overlay]");
    if (overlay) {
      overlay.addEventListener("mousedown", function (e) { if (e.target === overlay) closeDialog(); });
    }
    var first = host.querySelector(".editor-field");
    if (first) first.focus();
  }
  document.addEventListener("keydown", function (e) {
    if (e.key === "Escape" && document.querySelector("#editor-dialog [data-editor-overlay]")) {
      var open = document.querySelector("#editor-dialog .editor-hues[open]");
      if (open) { open.open = false; return; }
      closeDialog();
    }
  });

  function init(scope) {
    scope.querySelectorAll("[data-editor]").forEach(initEditor);
    scope.querySelectorAll("form[data-editor-toggle], form[data-basic-action]").forEach(initToggle);
  }
  document.addEventListener("DOMContentLoaded", function () {
    init(document);
    document.body.addEventListener("htmx:afterSwap", function (e) {
      init(e.target);
      if (e.target && e.target.id === "editor-dialog") initDialog(e.target);
    });
  });
})();
