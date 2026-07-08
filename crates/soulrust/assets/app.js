// Preserve each results table's scroll position across htmx swaps (a sort-column
// click or the 2s auto-refresh) so the table doesn't jump back to the first
// column or to the top. Keyed by the scroll container's index within the swapped
// `.results` container. Scroll position can't be preserved across a DOM
// replacement in CSS, so this is the one bit of app JS beyond htmx.
(function () {
  function save(t) {
    if (t && t.classList && t.classList.contains("results")) {
      t._scroll = Array.prototype.map.call(
        t.querySelectorAll(".results-scroll"),
        function (s) { return [s.scrollLeft, s.scrollTop]; }
      );
    }
  }
  function restore(t) {
    if (!t || !t._scroll) return;
    var els = t.querySelectorAll(".results-scroll");
    t._scroll.forEach(function (p, i) {
      if (els[i]) {
        els[i].scrollLeft = p[0];
        els[i].scrollTop = p[1];
      }
    });
  }
  // htmx events bubble to document, so listening here works regardless of when
  // this script runs relative to body parsing.
  document.addEventListener("htmx:beforeSwap", function (e) { save(e.target); });
  document.addEventListener("htmx:afterSwap", function (e) { restore(e.target); });
})();

// Mini player: clicking any [data-play-path] control streams that file into the
// sidebar <audio> element and plays it in the browser. Delegated on document so
// it keeps working for buttons rendered by htmx swaps.
(function () {
  document.addEventListener("click", function (e) {
    var el = e.target.closest("[data-play-path]");
    if (!el) return;
    e.preventDefault();
    var player = document.getElementById("player");
    if (!player) return;
    var wrap = document.getElementById("player-wrap");
    var np = document.getElementById("np");
    var path = el.getAttribute("data-play-path");
    var title = el.getAttribute("data-title") || "";
    if (wrap) wrap.hidden = false;
    if (np) { np.textContent = title; np.title = title; }
    player.src = "/media?path=" + encodeURIComponent(path);
    player.play().catch(function () {});
  });
})();
