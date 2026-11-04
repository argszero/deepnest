// The app's own chat window: one file an app's page includes, driving an agent that app declared.
//
// An app that declares an `[[agent]]` in its `deepnest.toml` has a real agent behind it — the turn
// runs inside deepnest, with the app's own directory as its working directory. What it has never
// had is a way for a person to talk to it. Writing that window from scratch in every app is how an
// app ends up with a text box that cannot be stopped, loses its conversation on reload, and renders
// the model's answer with `innerHTML`; so the window lives here, once, and an app asks for it with
// one line:
//
//     <script src="/agent-chat.js" data-agent="assistant"></script>
//
// What it does, and each of these is the reason it is not a text box:
//
// 1. **The answer arrives as it is written.** The turn is asked for with `Accept:
//    text/event-stream`, so the model's words appear the moment they exist instead of after the
//    turn has finished. A turn that uses tools runs for a minute or more; a window that waits for
//    the end is a window that looks broken for that minute.
// 2. **Reloading does not lose the conversation.** `GET /api/apps/<slug>/agent?agent=<name>` reads
//    the same `history.jsonl` the turn appends to, so the window opens on what was said rather than
//    on an empty page.
// 3. **Stop reaches the turn.** The turn is registered with deepnest's own cancel registry before
//    it starts, so `POST /api/apps/<slug>/cancel` ends it — including a model call already in
//    flight, which is abandoned at the socket rather than waited out.
// 4. **Text from anywhere is text.** The model's words and the person's are inserted with
//    `textContent`, never with `innerHTML`: an answer containing `<img onerror=...>` is an answer,
//    not markup.
//
// It calls three routes and brings no credential of its own: the browser presents the cookies the
// gateway minted when it served the app's page, scoped to those three paths. So a page whose app is
// stopped still opens its conversation, and the app's own server code needs no part in any of this.
//
// The agent's name comes from `data-agent`. Absent, the window says so and draws nothing: guessing
// which of three declared agents the person meant is worse than asking the app's author to say.

(function () {
  "use strict";

  if (window.deepnestAgentChatLoaded) {
    return;
  }
  window.deepnestAgentChatLoaded = true;

  var script = document.currentScript;
  var agent = (script && script.getAttribute("data-agent")) || "";
  var label = (script && script.getAttribute("data-label")) || "Assistant";

  // Where the app is. Two readings, and the second is not a fallback for a special case — it is the
  // one that matters: the meta is written into a page deepnest serves directly, and since the app
  // moved into a frame the frame's page carries no meta at all (the gateway injects the dialog into
  // the tab's page instead). What every shape does carry is the prefix it was served under,
  // `/app/<slug>/`, because that is the address the browser asked for. Reading it from the meta
  // first keeps the older single-page shape working without a second mechanism.
  var slug = (document.querySelector('meta[name="deepnest-app"]') || {}).content || "";

  if (!slug) {
    var served = /^\/app\/([^\/?#]+)\//.exec(window.location.pathname);
    if (served) {
      try {
        slug = decodeURIComponent(served[1]);
      } catch (error) {
        slug = served[1];
      }
    }
  }

  if (!agent) {
    return;
  }
  if (!slug) {
    // A page opened from the filesystem, or the app running on its own port while it is being
    // developed: none of the three routes exists there, so there is nothing to draw. Said out loud
    // rather than silently, because a window that does not appear is otherwise indistinguishable
    // from one that failed.
    if (window.console && console.warn) {
      console.warn("agent-chat: this page is not being served by deepnest, so there is nothing to talk to");
    }
    return;
  }

  var ROUTE = "/api/apps/" + encodeURIComponent(slug) + "/agent";
  var CANCEL = "/api/apps/" + encodeURIComponent(slug) + "/cancel";
  var STREAM = "text/event-stream";

  var host = document.body || document.documentElement;
  if (!host) {
    return;
  }

  function element(tag, className, text) {
    var node = document.createElement(tag);
    if (className) {
      node.className = className;
    }
    if (text !== undefined && text !== null) {
      node.textContent = String(text);
    }
    return node;
  }

  // ---- the shape -----------------------------------------------------------------------------

  var ball = element("button", "dnc-ball", "\u25cf");
  ball.type = "button";
  ball.setAttribute("aria-label", label);
  ball.setAttribute("aria-expanded", "false");
  ball.title = label;

  var panel = element("section", "dnc-panel");
  panel.hidden = true;
  panel.setAttribute("role", "dialog");
  panel.setAttribute("aria-label", label);

  var head = element("header", "dnc-head");
  var title = element("span", "dnc-title", label);
  var close = element("button", "dnc-close", "\u00d7");
  close.type = "button";
  close.setAttribute("aria-label", "Close");
  head.appendChild(title);
  head.appendChild(close);

  var transcript = element("div", "dnc-transcript");
  transcript.setAttribute("role", "log");
  transcript.setAttribute("aria-live", "polite");

  var state = element("div", "dnc-state");

  var form = element("form", "dnc-form");
  var input = element("textarea", "dnc-input");
  input.rows = 1;
  input.placeholder = "Message " + label + "\u2026";
  var send = element("button", "dnc-send", "Send");
  send.type = "submit";
  var stop = element("button", "dnc-stop", "Stop");
  stop.type = "button";
  stop.hidden = true;
  var acts = element("div", "dnc-acts");
  acts.appendChild(stop);
  acts.appendChild(send);
  form.appendChild(input);
  form.appendChild(acts);

  panel.appendChild(head);
  panel.appendChild(transcript);
  panel.appendChild(state);
  panel.appendChild(form);

  var root = element("div", "dnc-root");
  root.appendChild(panel);
  root.appendChild(ball);
  host.appendChild(root);

  function open(on) {
    panel.hidden = !on;
    ball.setAttribute("aria-expanded", on ? "true" : "false");
    if (on) {
      input.focus();
      scrollToEnd();
    }
  }

  function scrollToEnd() {
    transcript.scrollTop = transcript.scrollHeight;
  }

  function say(text, kind) {
    state.textContent = text || "";
    state.className = kind ? "dnc-state " + kind : "dnc-state";
  }

  {
    ball.addEventListener("click", function () {
      open(panel.hidden);
    });
    close.addEventListener("click", function () {
      open(false);
      ball.focus();
    });
    document.addEventListener("keydown", function (event) {
      if (event.key === "Escape" && !panel.hidden) {
        open(false);
        ball.focus();
      }
    });
  }

  // ---- one exchange on screen ----------------------------------------------------------------

  // A turn is drawn as up to three rows: what the person said, what the agent said in prose, and
  // one line about the work it did (how many tool calls, and whether the batch is waiting on the
  // app). The middle row is filled as the answer streams, so it is a node created before the
  // request and appended to afterwards.
  function row(kind) {
    var line = element("div", "dnc-row " + kind);
    transcript.appendChild(line);
    scrollToEnd();
    return line;
  }

  function drawTurn(turn) {
    if (turn.user) {
      row("dnc-user").textContent = turn.user;
    }
    if (turn.said && turn.said.length) {
      turn.said.forEach(function (piece) {
        var said = piece.text !== undefined ? piece.text : String(piece);
        if (said) {
          row("dnc-agent").textContent = said;
        }
      });
    }
    if (turn.reply) {
      row("dnc-agent").textContent = turn.reply;
    }
    if (turn.tools) {
      row("dnc-work").textContent =
        turn.tools === 1 ? "1 tool call" : turn.tools + " tool calls";
    }
  }

  // ---- history -------------------------------------------------------------------------------

  function load() {
    var url = ROUTE + "?agent=" + encodeURIComponent(agent);
    return fetch(url, { headers: { accept: "application/json" }, cache: "no-store" })
      .then(function (response) {
        if (!response.ok) {
          return response.text().then(function (text) {
            throw new Error(reason(text, response.status));
          });
        }
        return response.json();
      })
      .then(function (answer) {
        transcript.textContent = "";
        var turns = (answer && answer.turns) || [];
        if (!turns.length) {
          var empty = row("dnc-empty");
          empty.textContent = "Nothing here yet. Say what you need.";
          return;
        }
        turns.forEach(drawTurn);
      })
      .catch(function (error) {
        // A conversation that could not be read is not an empty conversation, and drawing it as one
        // would tell the person their history was never there.
        transcript.textContent = "";
        row("dnc-error").textContent = "Could not read the conversation: " + message(error);
      });
  }

  function reason(text, status) {
    try {
      var body = JSON.parse(text);
      return body.error || body.detail || status + " " + text;
    } catch (error) {
      return text || String(status);
    }
  }

  function message(error) {
    return String((error && error.message) || error);
  }

  // ---- sending -------------------------------------------------------------------------------

  var running = false;

  function busy(on) {
    running = on;
    send.hidden = on;
    stop.hidden = !on;
    input.disabled = on;
    if (!on) {
      input.focus();
    }
  }

  function sendMessage(text) {
    var mine = row("dnc-user");
    mine.textContent = text;
    var answer = row("dnc-agent");
    var typed = "";
    busy(true);
    say("Working\u2026");

    var body = JSON.stringify({
      agent: agent,
      message: text,
      // The app's own tools travel with the request, so a turn never has to call back into the app
      // it is running inside — the deadlock the batch hand-back exists to prevent. An app with no
      // tools sends none.
      tools: window.deepnestAgentTools || undefined,
    });

    return fetch(ROUTE, {
      method: "POST",
      headers: { "content-type": "application/json", accept: STREAM },
      cache: "no-store",
      body: body,
    }).then(function (response) {
      var type = response.headers.get("content-type") || "";
      if (type.indexOf(STREAM) === -1) {
        // A refusal is never a stream. Read the body as the answer it is, and say it as one.
        return response.text().then(function (text) {
          if (!response.ok) {
            throw new Error(reason(text, response.status));
          }
          var whole = JSON.parse(text);
          draw(answer, whole);
          reply(whole);
        });
      }
      return readEvents(response, function (piece) {
        typed += piece;
        answer.textContent = typed;
        scrollToEnd();
      }).then(function (final) {
        // The stream's closing event carries the same body the JSON path answers, so what the turn
        // did is read from one place rather than reconstructed from the pieces.
        if (!typed && final && final.text) {
          answer.textContent = final.text;
        }
        reply(final);
      });
    }).catch(function (error) {
      if (!typed) {
        answer.remove();
      }
      var failed = row("dnc-error");
      if (error && error.stopped) {
        failed.textContent = "Stopped.";
      } else {
        failed.textContent = message(error);
      }
    }).then(function () {
      busy(false);
      say("");
      scrollToEnd();
    });
  }

  function draw(answer, body) {
    if (body && body.text) {
      answer.textContent = body.text;
    } else {
      answer.remove();
    }
  }

  function reply(body) {
    if (!body) {
      return;
    }
    if (body.outcome === "paused") {
      // The model asked the app to do something. Nothing in this window can run it, so the row says
      // what it asked for rather than pretending the turn finished.
      var names = (body.pending || []).map(function (call) {
        return call.name;
      });
      row("dnc-work").textContent =
        "waiting on the app to run: " + (names.join(", ") || "a tool");
      return;
    }
    if (body.cancelled) {
      row("dnc-work").textContent = "stopped";
    }
  }

  // `event: <kind>` and `data: <json>`, one blank line to end each. The kinds are `text` for a
  // piece of the answer, then exactly one of `done` or `error` carrying the whole reply.
  function readEvents(response, onText) {
    var reader = response.body.getReader();
    var decoder = new TextDecoder();
    var buffered = "";
    var final = null;

    function take(block) {
      var kind = null;
      var data = "";
      block.split("\n").forEach(function (line) {
        if (line.indexOf("event: ") === 0) {
          kind = line.slice(7);
        } else if (line.indexOf("data: ") === 0) {
          data += line.slice(6);
        }
      });
      if (!kind) {
        return;
      }
      var body = null;
      try {
        body = JSON.parse(data);
      } catch (error) {
        return;
      }
      if (kind === "text" && body && typeof body.text === "string") {
        onText(body.text);
      } else if (kind === "done") {
        final = body;
      } else if (kind === "error") {
        var error = new Error((body && (body.error || body.detail)) || "the turn failed");
        error.body = body;
        throw error;
      }
    }

    function pump() {
      return reader.read().then(function (step) {
        if (step.done) {
          if (buffered.trim()) {
            take(buffered);
          }
          return final;
        }
        buffered += decoder.decode(step.value, { stream: true });
        var cut;
        while ((cut = buffered.indexOf("\n\n")) !== -1) {
          take(buffered.slice(0, cut));
          buffered = buffered.slice(cut + 2);
        }
        return pump();
      });
    }

    return pump();
  }

  form.addEventListener("submit", function (event) {
    event.preventDefault();
    var text = input.value.trim();
    if (!text || running) {
      return;
    }
    input.value = "";
    sendMessage(text);
  });

  input.addEventListener("keydown", function (event) {
    if (event.key === "Enter" && !event.shiftKey) {
      event.preventDefault();
      form.dispatchEvent(new Event("submit", { cancelable: true }));
    }
  });

  stop.addEventListener("click", function () {
    stop.disabled = true;
    say("Stopping\u2026");
    fetch(CANCEL, { method: "POST", cache: "no-store" })
      .then(function (response) {
        return response.text().then(function (text) {
          var body = null;
          try {
            body = JSON.parse(text);
          } catch (error) {
            body = null;
          }
          // Zero turns to stop is an ordinary answer — the turn may have ended on its own — so it is
          // reported as it is rather than as a failure.
          say(body && body.note ? body.note : "asked to stop");
        });
      })
      .catch(function (error) {
        say("Could not stop: " + message(error), "bad");
      })
      .then(function () {
        stop.disabled = false;
      });
  });

  load();
})();
