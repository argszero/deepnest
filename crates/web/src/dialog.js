// The floating dialog, injected into every page an app answers.
//
// The gateway injects it rather than the agent writing it into the app, so that an agent rewriting
// the app's page wholesale cannot delete it: this ball is the only way back to the control panel
// from a page the user is looking at.
//
// What this file deliberately does not do:
//
// 1. **It never holds the control token.** The app's own script shares this origin, so a token
//    written into this page is a token the app can read and spend — and the control token spends
//    on every route there is.
// 2. **It calls three routes and brings no credential of its own**: `POST /api/apps/<slug>/chat`,
//    `GET`/`POST /api/apps/<slug>/escalation` — the second is how a widening request reaches the
//    person in front of the page — and `GET /api/apps/<slug>/build`, which is what a turn that
//    changed the app leaves behind to be watched. For each, the browser attaches a cookie the
//    server minted when it served this page, scoped to that one path, and all three are different
//    values. So the box works on a page whose app is stopped or mid-rewrite, and a page that
//    cannot present that cookie gets a `401` and says so instead of failing silently.
//
// The residual, stated where it is made rather than left implied: the app's own script can make
// that same request, so an app can spend its own agent turns. It cannot spend another app's, reach
// the config, start or stop anything, or read the token. That is the difference between narrowing a
// capability and handing one over, and it is why the cookie names a route rather than an API.
//
// Text from anywhere — an app's slug is a directory name, and the agent's answer is the model's
// words — is inserted with `textContent`, never with `innerHTML`, for the same reason the panel's
// script does it that way.

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

  // The message box: a form so Enter-to-send and the platform's own keyboard handling come for
  // free, and a submit handler rather than a click handler so a keyboard user is not a second path.
  var form = document.createElement("form");
  form.id = "deepnest-dialog-form";

  var input = document.createElement("textarea");
  input.id = "deepnest-dialog-input";
  input.rows = 3;
  input.placeholder = "Ask for a change…";
  input.setAttribute("aria-label", "Message to this app's agent");

  var send = document.createElement("button");
  send.type = "submit";
  send.id = "deepnest-dialog-send";
  send.textContent = "Send";

  // `role="status"` with `aria-live` announces the answer without moving focus out of the box,
  // so a second message can follow the first.
  var answer = document.createElement("p");
  answer.id = "deepnest-dialog-answer";
  answer.hidden = true;
  answer.setAttribute("role", "status");
  answer.setAttribute("aria-live", "polite");

  // The widening request. It appears only while a turn is waiting for one, which is why it is a
  // block that starts hidden rather than something the user opens: there is nothing to open when
  // nobody is asking, and a permanent button that says "approve" would be a button with no object.
  var asked = document.createElement("div");
  asked.id = "deepnest-dialog-asked";
  asked.hidden = true;

  var askedWhat = document.createElement("p");
  askedWhat.id = "deepnest-dialog-asked-what";

  var askedWhy = document.createElement("p");
  askedWhy.id = "deepnest-dialog-asked-why";

  var allow = document.createElement("button");
  allow.type = "button";
  allow.id = "deepnest-dialog-allow";
  allow.textContent = "Allow for this call";

  var refuse = document.createElement("button");
  refuse.type = "button";
  refuse.id = "deepnest-dialog-refuse";
  refuse.textContent = "No";

  // Why not. A bare no is the one answer the agent cannot act on: `Denied` and `Unanswered` are
  // separate answers because the model's next move differs, and a refusal without words leaves it
  // to guess which it was given. Optional, and an empty field sends no `reason` key at all rather
  // than an empty string — a person who just wants to say no says exactly that.
  var whyNot = document.createElement("textarea");
  whyNot.id = "deepnest-dialog-reason";
  whyNot.rows = 2;
  whyNot.placeholder = "Why not? Optional — the agent reads this and changes course.";
  whyNot.setAttribute("aria-label", "Why you are refusing");

  asked.appendChild(askedWhat);
  asked.appendChild(askedWhy);
  asked.appendChild(whyNot);
  asked.appendChild(allow);
  asked.appendChild(refuse);

  // The block and the words typed into it go away together: a reason left behind would answer the
  // next question with the previous one's refusal.
  function clearTheQuestion() {
    asked.hidden = true;
    whyNot.value = "";
  }

  form.appendChild(input);
  form.appendChild(send);

  var link = document.createElement("a");
  link.id = "deepnest-dialog-link";
  link.href = "/";
  link.textContent = "Open the control panel";

  // The conversation, read back from the server rather than remembered here. That is what makes
  // it survive the reload a finished rebuild performs: the page comes back empty and asks again,
  // and the answer it was showing is in the file the agent wrote, not in a variable that died.
  var transcript = document.createElement("div");
  transcript.id = "deepnest-dialog-transcript";
  transcript.setAttribute("role", "log");
  transcript.setAttribute("aria-label", "This app's conversation");

  panel.appendChild(title);
  panel.appendChild(note);
  panel.appendChild(transcript);
  panel.appendChild(form);
  panel.appendChild(asked);
  panel.appendChild(answer);
  panel.appendChild(link);
  root.appendChild(panel);
  root.appendChild(ball);
  host.appendChild(root);

  function show(open) {
    panel.hidden = !open;
    ball.setAttribute("aria-expanded", open ? "true" : "false");
    if (open) {
      // Read before focus: the transcript is what the person came to see, and asking for it here
      // is also what makes the page's first open after a rebuild show the turn that caused it.
      readHistory();
      input.focus();
    }
  }

  // Put a line under the box. `bad` is a class rather than a colour inline: the stylesheet is where
  // colours live, and `all: initial` means nothing here inherits one.
  function say(text, bad) {
    answer.textContent = text;
    answer.hidden = false;
    if (bad) {
      answer.classList.add("bad");
    } else {
      answer.classList.remove("bad");
    }
  }

  // The question route: one path, one cookie, and nothing about it needs the control token.
  function askingRoute() {
    return "/api/apps/" + encodeURIComponent(slug) + "/escalation";
  }

  // The conversation route: a read, and the only thing here that is.
  var TURNS_SHOWN = 20;

  function historyRoute() {
    return "/api/apps/" + encodeURIComponent(slug) + "/history";
  }

  // One turn is: what was asked, what the agent did about it, and what it said back. Rendered with
  // `textContent` throughout — every string here is either the person's words or the model's.
  function renderTurn(turn) {
    var line = document.createElement("div");
    line.className = "deepnest-turn";

    var asked = document.createElement("p");
    asked.className = "deepnest-asked";
    asked.textContent = turn.user;

    var did = document.createElement("p");
    did.className = "deepnest-did";
    if (turn.tools > 0) {
      did.textContent =
        "ran " + turn.tools + (turn.tools === 1 ? " tool" : " tools");
    }

    line.appendChild(asked);
    if (did.textContent) {
      line.appendChild(did);
    }
    if (turn.reply) {
      var said = document.createElement("p");
      said.className = "deepnest-said";
      said.textContent = turn.reply;
      line.appendChild(said);
    }
    // The agent's own notes: why a turn ended early, what it could not run. This is the line the
    // user needed and could not see — a turn that spent its budget on tools has no answer, and
    // without the note it is indistinguishable from a turn that never ran.
    (turn.notes || []).forEach(function (text) {
      var noteLine = document.createElement("p");
      noteLine.className = "deepnest-note";
      noteLine.textContent = text;
      line.appendChild(noteLine);
    });
    return line;
  }

  function showTurns(turns) {
    transcript.textContent = "";
    if (!turns || turns.length === 0) {
      transcript.hidden = true;
      return;
    }
    var recent = turns.slice(-TURNS_SHOWN);
    recent.forEach(function (turn) {
      transcript.appendChild(renderTurn(turn));
    });
    transcript.hidden = false;
    // Newest at the bottom, which is where a conversation is read from.
    transcript.scrollTop = transcript.scrollHeight;
  }

  function readHistory() {
    return fetch(historyRoute(), { credentials: "same-origin" })
      .then(function (response) {
        return response.ok ? response.json() : null;
      })
      .then(function (body) {
        showTurns(body && body.turns);
      })
      .catch(function () {
        // A transcript nobody can read is not a reason to break the box that still works.
      });
  }

  // Polling rather than a stream: a turn holds its own connection open while it waits, the answer
  // is one bit, and a poll every second costs this server nothing — where a second streaming
  // adapter would be another thing to get wrong on a page the app may have rewritten.
  var polling = null;

  function stopPolling() {
    if (polling !== null) {
      clearInterval(polling);
      polling = null;
    }
    clearTheQuestion();
  }

  function pollForAQuestion() {
    fetch(askingRoute(), { credentials: "same-origin" })
      .then(function (response) {
        // A `401` here is the same reading as on the box: the cookie is gone. It is not worth a
        // line under the answer — the box will say it when the turn ends — so the question simply
        // does not appear.
        return response.ok ? response.json() : null;
      })
      .then(function (body) {
        var pending = body && body.pending;
        if (!pending) {
          // Only the block: a reason half-typed while the poll found nothing is still the person's,
          // and a poll every 700 ms must not wipe it.
          asked.hidden = true;
          return;
        }
        // Both halves in the user's terms: what is being asked for, and why. `textContent`
        // throughout — the justification is the model's words.
        askedWhat.textContent =
          "This needs " + pending.to + " instead of " + pending.from + ", for one call.";
        askedWhy.textContent = pending.justification;
        asked.hidden = false;
      })
      .catch(function () {
        // A poll that failed is a poll that failed. The turn's own request is what reports a
        // server that has gone away.
      });
  }

  // The build route, and the end of the turn's sentence. The turn said `changed` and returned; a
  // build started on a thread of the server's own. `ok` means a new binary is serving, which is
  // this page's cue to reload onto it; `failed` carries the compiler's last lines, because "it did
  // not work" without them leaves a person with nothing to say next.
  var REBUILD_PATIENCE = 5 * 60 * 1000;

  function watchTheRebuild() {
    watch(routeFor("build"), REBUILD_PATIENCE, Date.now());
  }

  function routeFor(action) {
    return "/api/apps/" + encodeURIComponent(slug) + "/" + action;
  }

  function watch(route, patience, started) {
    fetch(route, { credentials: "same-origin" })
      .then(function (response) {
        return response.ok ? response.json() : null;
      })
      .then(function (body) {
        // No answer — a `401` after the cookie expired, or a server that went away. The next tick
        // asks again, and past the patience the line below says so rather than waiting forever.
        if (body) {
          if (body.state === "ok") {
            location.reload();
            return;
          }
          if (body.state === "failed") {
            say(
              "The build failed, so the app is still running what it ran before." +
                (body.detail ? "\n\n" + body.detail : ""),
              true
            );
            return;
          }
          if (body.state === "building") {
            say("Rebuilding…", false);
          }
        }
        if (Date.now() - started > patience) {
          say("The rebuild is taking longer than a build should — open the control panel to see why.", true);
          return;
        }
        setTimeout(function () {
          watch(route, patience, started);
        }, 700);
      })
      .catch(function () {
        if (Date.now() - started > patience) {
          return;
        }
        setTimeout(function () {
          watch(route, patience, started);
        }, 700);
      });
  }

  function startPolling() {
    if (polling !== null) {
      return;
    }
    pollForAQuestion();
    polling = setInterval(pollForAQuestion, 700);
  }

  function answerTheQuestion(approved) {
    allow.disabled = true;
    refuse.disabled = true;
    // The words travel only with a refusal, and only when there are some. `Approved` has nothing to
    // explain, and the verdict the route reads refuses a key it does not know — so an approval
    // sends the one key it needs and no other.
    var verdict = { approve: approved };
    if (!approved) {
      var whyText = whyNot.value.trim();
      if (whyText) {
        verdict.reason = whyText;
      }
    }
    fetch(askingRoute(), {
      method: "POST",
      credentials: "same-origin",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify(verdict)
    })
      .then(function () {
        // Whether it landed or not, the question is no longer this box's to show: the turn either
        // woke with the answer or stopped waiting, and the next poll would be about a request that
        // is already gone.
        clearTheQuestion();
      })
      .catch(function () {
        clearTheQuestion();
      })
      .then(function () {
        allow.disabled = false;
        refuse.disabled = false;
      });
  }

  function ask(text) {
    return fetch("/api/apps/" + encodeURIComponent(slug) + "/chat", {
      method: "POST",
      // Same-origin is what attaches the cookie this page was given, and it is the default — but
      // spelled out, because the whole mechanism rests on it and a later `credentials: "omit"`
      // would turn the box into a permanent `401`.
      credentials: "same-origin",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ message: text })
    });
  }

  form.addEventListener("submit", function (event) {
    event.preventDefault();
    var text = input.value.trim();
    if (!text) {
      return;
    }
    if (!slug) {
      // Without a slug there is no route to send to. Saying so beats posting to `/api/apps//chat`.
      say("This page does not say which app it is — open it from the control panel.", true);
      return;
    }

    send.disabled = true;
    say("Asking…", false);
    // The turn may stop and ask a question before it answers, so the poll starts with the
    // question already possible rather than after the first answer.
    startPolling();

    ask(text)
      .then(function (response) {
        // A refusal is JSON in every case this server produces; a proxy's error page is not, and
        // the status is then the only thing left to report.
        return response.json().then(
          function (body) {
            return { status: response.status, body: body };
          },
          function () {
            return { status: response.status, body: null };
          }
        );
      })
      .then(function (result) {
        send.disabled = false;
        stopPolling();
        if (result.status === 200 && result.body && typeof result.body.text === "string") {
          var text = result.body.text;
          if (result.body.changed) {
            // The turn came back as soon as it had written; the build it started is still running.
            // Saying "reload to see it" was the whole defect — nothing had been built to see — so
            // the line names what is happening and `watchTheRebuild` finishes the sentence.
            text += "\n\nThis changed the app — rebuilding it now.";
          }
          input.value = "";
          say(text, false);
          // The turn is on disk the moment the route answers, so this reads what was actually
          // recorded rather than echoing what came back in the reply.
          readHistory();
          if (result.body.changed) {
            watchTheRebuild();
          }
          return;
        }
        if (result.status === 401) {
          // The grant is gone: expired, or this page was not served by this process. Reloading is
          // the one action that mints another.
          say("This page's permission to send has expired — reload the page and ask again.", true);
          return;
        }
        var why =
          result.body && typeof result.body.error === "string"
            ? result.body.error
            : "the request failed (" + result.status + ")";
        say(why, true);
      })
      .catch(function () {
        send.disabled = false;
        stopPolling();
        say("The request did not reach deepnest. Is it still running?", true);
      });
  });

  allow.addEventListener("click", function () {
    answerTheQuestion(true);
  });

  refuse.addEventListener("click", function () {
    answerTheQuestion(false);
  });

  ball.addEventListener("click", function () {
    show(panel.hidden);
  });

  // The page a finished rebuild reloads onto has no memory of the turn that started it, so this
  // is where the conversation comes back from: the file, not the tab.
  readHistory();

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
