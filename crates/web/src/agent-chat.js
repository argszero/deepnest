// The app's own chat window, switched on by one line of the app's markup — and this file is the
// initializer, not the window.
//
// An app that declares an `[[agent]]` in its `deepnest.toml` has a real agent behind it: the turn
// runs inside deepnest, with the app's own directory as its working directory. What it needs is a
// way for a person to talk to it, and the shape of the answer is the point of this file. Writing a
// second window here — a second transcript, a second streaming reader, a second Stop — produced a
// window that could not compact, could not ask for a wider sandbox, and could not take a
// screenshot: 471 lines against the dialog's 3,646, and every capability not yet written twice.
// So there is one window. `/dialog.js` is it, and this file's whole job is to say which
// conversation it should hold:
//
//     <script src="/agent-chat.js" data-agent="assistant"></script>
//
// `data-agent` is the name from the app's `[[agent]]` table and `data-label` is what the window is
// called. Absent a name, nothing is drawn and the console says so: guessing which of three declared
// agents the person meant is worse than asking the app's author to say.
//
// What the dialog then does on this channel, none of which this file has to arrange:
//
// 1. **The answer arrives as it is written.** It asks for the turn with `Accept:
//    text/event-stream`, and the agent route answers with the same `event: text` … `event: done`
//    stream the app's control conversation uses — one writer in the router, two routes.
// 2. **Reloading does not lose the conversation.** It reads `GET /api/apps/<slug>/agent?agent=<name>`,
//    the same `history.jsonl` the turn appends to, in the same `{turns: […]}` shape `/history`
//    answers.
// 3. **Stop reaches the turn.** `POST /api/apps/<slug>/cancel` is the same registry the app's own
//    turns are in, because what a person means by "stop" is "stop this app's work" whoever started
//    it.
// 4. **Text from anywhere is text.** The dialog inserts every sentence with `textContent`, never
//    with `innerHTML`.
//
// It brings no credential of its own: the browser presents the cookies the gateway minted when it
// served the app's page, scoped to those routes. So a page whose app is stopped still opens its
// conversation, and the app's own server code needs no part in any of this.

(function () {
  "use strict";

  if (window.deepnestAgentChatLoaded) {
    return;
  }
  window.deepnestAgentChatLoaded = true;

  var script = document.currentScript;
  var agent = (script && script.getAttribute("data-agent")) || "";
  var label = (script && script.getAttribute("data-label")) || "Assistant";

  if (!agent) {
    // A window that does not appear is otherwise indistinguishable from one that failed, so the
    // reason is said out loud rather than left to the empty corner of the page.
    if (window.console && console.warn) {
      console.warn(
        "agent-chat: this tag names no agent — add data-agent=\"<the name in the app's [[agent]] table>\""
      );
    }
    return;
  }

  // Read by `/dialog.js` when it boots. Set here, before the script below is appended, because that
  // is the whole of the handover: this file decides *which conversation*, the dialog decides
  // everything about how a conversation is drawn.
  window.deepnestDialogTarget = { agent: agent, label: label };

  // The dialog's own stylesheet: one window means one set of styles, and a second one beside it is
  // how the two windows drifted apart in the first place.
  if (!document.querySelector('link[data-deepnest-dialog-style]')) {
    var style = document.createElement("link");
    style.rel = "stylesheet";
    style.href = "/dialog.css";
    style.setAttribute("data-deepnest-dialog-style", "");
    (document.head || document.documentElement).appendChild(style);
  }

  // The renderer itself, once. A page that already carries the dialog — the stage page does — has
  // nothing to add, and `window.deepnestDialogLoaded` is the flag its own copy sets.
  if (!window.deepnestDialogLoaded && !document.querySelector("script[data-deepnest-dialog]")) {
    var renderer = document.createElement("script");
    renderer.src = "/dialog.js";
    renderer.setAttribute("data-deepnest-dialog", "");
    (document.head || document.documentElement).appendChild(renderer);
  }
})();
