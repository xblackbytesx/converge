// Converge: the few client-side bits HTMX does not cover. No inline
// handlers (the CSP forbids them): everything hangs off data- attributes,
// delegated from document so swapped-in fragments work too.
(function () {
  "use strict";

  // <button data-copy="#selector">: copy that element's text.
  document.addEventListener("click", function (e) {
    var btn = e.target.closest("[data-copy]");
    if (!btn) return;
    var src = document.querySelector(btn.getAttribute("data-copy"));
    if (!src || !navigator.clipboard) return;
    navigator.clipboard.writeText(src.textContent.trim()).then(function () {
      var old = btn.textContent;
      btn.textContent = "Copied";
      setTimeout(function () { btn.textContent = old; }, 1200);
    });
  });

  // <form data-confirm="Question?">: ask before a plain (non-HTMX) submit.
  document.addEventListener("submit", function (e) {
    var form = e.target;
    if (form.matches && form.matches("form[data-confirm]") && !window.confirm(form.getAttribute("data-confirm"))) {
      e.preventDefault();
    }
  });

  // A page that runs on its own script (the Playground) must never arrive as
  // an HTMX swap: HTMX does not run swapped-in scripts. Links to it opt out
  // of boosting; if one is missed, load the page for real once.
  document.addEventListener("htmx:afterSettle", function () {
    var pg = document.getElementById("pg");
    if (pg && !pg.hasAttribute("data-ready")) window.location.reload();
  });

  // Clear the flash box when a new request starts, so old errors do not linger.
  document.addEventListener("htmx:beforeRequest", function () {
    var f = document.getElementById("flash");
    if (f) f.innerHTML = "";
  });
})();
