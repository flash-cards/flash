// Study-view keyboard shortcuts: space reveals, 1-4 grades. In the
// typed-answer input, Enter reveals (and other shortcuts stay inert so
// typing "1" doesn't grade).
document.addEventListener("keydown", function (e) {
  if (e.target && e.target.id === "type-input") {
    if (e.key === "Enter") {
      e.preventDefault();
      var btn = document.getElementById("reveal-btn");
      if (btn) btn.click();
    }
    return;
  }
  if (e.target.tagName === "INPUT" || e.target.tagName === "TEXTAREA") return;
  var el = null;
  if (e.key === " ") el = document.getElementById("reveal-btn");
  if (e.key >= "1" && e.key <= "4") el = document.getElementById("grade-" + e.key);
  if (el) { e.preventDefault(); el.click(); }
});
