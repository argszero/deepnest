// The floating dialog, injected into every page an app answers.
//
// The gateway injects it rather than the agent writing it into the app, so that an agent rewriting
// the app's page wholesale cannot delete it: this ball is the only way back to the control panel
// from a page the user is looking at.
//
// Two things this file deliberately does not do:
//
// 1. **It carries no control token.** The app's own script shares this origin, so a token written
//    into this page is a token the app could read and spend. Everything the dialog shows is either
//    in the page the gateway wrote or is the panel the browser loads itself.
// 2. **It fetches nothing.** There is no `/api/` call here, so a dialog on a page whose app is
//    stopped, wedged or mid-rewrite still opens.
//
// Text from anywhere — an app's slug is a directory name — is inserted with `textContent`, never
// with `innerHTML`, for the same reason the panel's script does it that way.

(function () {
  "use strict";

  // The gateway's markup already declines to inject a second copy, but a page can be assembled
  // from fragments that each carry one, and the script is the last place a second ball can be
  // stopped.
  if (window.deepnestDialogLoaded) {
    return;
  }
  window.deepnestDialogLoaded = true;

  var host = document.body || document.documentElement;
  if (!host) {
    return;
  }

  var meta = document.querySelector('meta[name="deepnest-app"]');
  var slug = meta ? meta.getAttribute("content") || "" : "";

  var root = document.createElement("div");
  root.id = "deepnest-dialog";

  var ball = document.createElement("button");
  ball.type = "button";
  ball.id = "deepnest-dialog-ball";
  ball.setAttribute("aria-label", "deepnest");
  ball.setAttribute("aria-expanded", "false");
  ball.textContent = "d";

  var panel = document.createElement("div");
  panel.id = "deepnest-dialog-panel";
  panel.hidden = true;

  var title = document.createElement("p");
  title.id = "deepnest-dialog-title";
  title.textContent = slug || "this app";

  var note = document.createElement("p");
  note.id = "deepnest-dialog-note";
  note.textContent = "Made by deepnest and running on this machine.";

  var link = document.createElement("a");
  link.id = "deepnest-dialog-link";
  link.href = "/";
  link.textContent = "Open the control panel";

  panel.appendChild(title);
  panel.appendChild(note);
  panel.appendChild(link);
  root.appendChild(panel);
  root.appendChild(ball);
  host.appendChild(root);

  function show(open) {
    panel.hidden = !open;
    ball.setAttribute("aria-expanded", open ? "true" : "false");
    if (open) {
      link.focus();
    }
  }

  ball.addEventListener("click", function () {
    show(panel.hidden);
  });

  document.addEventListener("keydown", function (event) {
    if (event.key === "Escape" && !panel.hidden) {
      show(false);
      ball.focus();
    }
  });

  // Capture, so a click the app stopped from propagating still closes the panel — an app that
  // calls `stopPropagation` on every click would otherwise leave the panel open for good.
  document.addEventListener(
    "click",
    function (event) {
      if (!panel.hidden && !root.contains(event.target)) {
        show(false);
      }
    },
    true
  );
})();
