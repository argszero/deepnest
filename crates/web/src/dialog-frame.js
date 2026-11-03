// The app's own document, as far as the dialog needs it.
//
// The dialog stands on deepnest's page and the app is a frame inside it, which is what stops a
// finished rebuild from taking the conversation with it. But a frame is a document boundary, and two
// things the dialog had while it stood inside the app's page are now on the wrong side of it: the
// chord that opens the panel, and the click that closes it. Neither crosses a boundary, and the
// dialog can put this script into the frame itself — same origin — which is what it does on every
// load.
//
// Deliberately tiny, and deliberately not the dialog. The app's page is the agent's to rewrite, and
// the less of deepnest that stands in it, the less an app's own scripts and its own markup can
// interfere with.
(function () {
  "use strict";

  // This document's origin, which is the parent's too: both are served from deepnest's single port.
  // Named rather than `"*"`, so a frame that somehow found itself on another origin could not have
  // its messages read by whoever is on the other side.
  var origin = window.location.origin;

  function tell(kind) {
    try {
      window.parent.postMessage({ deepnest: "frame", kind: kind }, origin);
    } catch (error) {
      // A parent that is gone, or one this document may not speak to. There is nothing to do about
      // it here, and nothing the person needs to be told: the ball is on the other side.
    }
  }

  // The keyboard way in, with the guards the dialog's own handler has. The chord belongs to the app
  // when the app says so, and an input method turning `k` into half a character is not a chord.
  document.addEventListener("keydown", function (event) {
    if (!event.metaKey && !event.ctrlKey) {
      return;
    }
    if (event.altKey || event.shiftKey || event.defaultPrevented || event.isComposing) {
      return;
    }
    if (String(event.key).toLowerCase() !== "k") {
      return;
    }
    event.preventDefault();
    tell("chord");
  });

  // The click that closes the panel: anything the person clicks in the app is outside a panel that
  // is drawn over it. Capture, for the reason the dialog's own handler captures — an app that stops
  // propagation on every click would otherwise leave the panel open for good.
  document.addEventListener(
    "click",
    function () {
      tell("press");
    },
    true
  );
})();
