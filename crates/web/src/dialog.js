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
// 2. **It calls five routes and brings no credential of its own**: `POST /api/apps/<slug>/chat`,
//    `GET`/`POST /api/apps/<slug>/escalation` — the second is how a widening request reaches the
//    person in front of the page — `GET /api/apps/<slug>/build`, which is what a turn that changed
//    the app leaves behind to be watched, `GET /api/apps/<slug>/history`, which is what makes the
//    conversation survive a reload, and `POST /api/apps/<slug>/cancel`, which stops a turn that is
//    already spending. For each, the browser attaches a cookie the server minted when it served
//    this page, scoped to that one path, and all five are different
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

  // Stop. It appears only while a turn is running, for the same reason the widening block starts
  // hidden: a permanent button that says "stop" is a button with no object. `type="button"` because
  // it sits inside the form, and a stop that submitted the form would start a second turn.
  var stop = document.createElement("button");
  stop.type = "button";
  stop.id = "deepnest-dialog-stop";
  stop.textContent = "Stop";
  stop.hidden = true;

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

  // Pictures in the message, as markers where the person put them.
  //
  // The marker is the whole mechanism. A picture parked beside the box loses the one thing the
  // question needs — which words it belongs to — so "this bit is wrong, do it like that one"
  // arrives as two pictures and some prose with nothing tying either to the sentence it is about.
  // Typed into the text at the caret, `[image 1]` sits exactly where the reference is, travels in
  // the message body itself, and is what the transcript shows afterwards.
  //
  // The bytes stay out of the text, in `attached`: a `data:` URL typed into the box would be a
  // screen of base64 to edit around, and the request already has a field for them. What links the
  // two is the number, and a number is never reused, so deleting one marker cannot quietly attach
  // the next picture to the previous reference.
  var MAX_PICTURES = 8;
  var attached = [];
  var nextPicture = 1;

  // Files, the same mechanism with the name in place of the number. `[file report.csv]` is the
  // marker because a picture has no name and a file does: the name is what the person is pointing
  // at, and it is what the model matches the contents below the message to. The bytes stay here in
  // `carried` and travel as base64 in the request; the marker is the only way to reach them, so a
  // marker cut out of the words is a file that is not sent.
  var MAX_FILES = 8;
  var carried = [];

  // A live region of its own rather than a line in the answer box: the answer box is where the
  // last turn's words are, and taking a picture arrives before any turn has run. It says what is
  // in the message rather than showing it, because the markers are the picture's place and a
  // person who cannot see the message still has to know how many travelled with it.
  var pictureNote = document.createElement("p");
  pictureNote.id = "deepnest-dialog-picture-note";
  pictureNote.setAttribute("role", "status");
  pictureNote.setAttribute("aria-live", "polite");
  pictureNote.hidden = true;

  function markerFor(id) {
    return "[image " + id + "]";
  }

  function fileMarkerFor(name) {
    return "[file " + name + "]";
  }

  // The pictures the message still refers to, in the order the words mention them.
  //
  // Read out of the text rather than out of `attached`: a marker that was deleted is a picture the
  // person decided against, and a marker that survived is one they kept. Ordering by first mention
  // is what makes the numbers in the words and the order in the request agree, and a marker the
  // person happens to repeat still sends its bytes once.
  function picturesReferredTo(text) {
    var pattern = /\[image (\d+)\]/g;
    var found = [];
    var taken = {};
    var match = pattern.exec(text);
    while (match !== null) {
      var id = Number(match[1]);
      if (!taken[id]) {
        for (var index = 0; index < attached.length; index += 1) {
          if (attached[index].id === id) {
            taken[id] = true;
            found.push(attached[index].url);
            break;
          }
        }
      }
      match = pattern.exec(text);
    }
    return found;
  }

  // The files the words still name, in the order they name them — the same reading as the
  // pictures, and for the same reason: what the person kept in the text is what they meant to send.
  // A name is only sent once however often it is mentioned, because pointing at one file twice does
  // not make it two files.
  function filesReferredTo(text) {
    var pattern = /\[file ([^\]\n]+)\]/g;
    var found = [];
    var taken = {};
    var match = pattern.exec(text);
    while (match !== null) {
      var name = match[1];
      if (!taken[name]) {
        for (var index = 0; index < carried.length; index += 1) {
          if (carried[index].name === name) {
            taken[name] = true;
            found.push(carried[index]);
            break;
          }
        }
      }
      match = pattern.exec(text);
    }
    return found;
  }

  // Sent, so the pictures go with it: one left behind would ride along with the next message too,
  // and a conversation that accumulates screenshots nobody chose to resend is a conversation that
  // costs the user tokens for the rest of the app's life. The numbering does not restart, because a
  // marker that outlived its picture would then name the wrong one.
  function forgetAttachments() {
    attached = [];
    carried = [];
    refreshNote();
  }

  function counted(count, noun) {
    return count + " " + noun + (count === 1 ? "" : "s");
  }

  // One sentence over both kinds, because they travel in one message: "1 picture and 2 files in the
  // message, each where its marker is."
  function refreshNote() {
    var pictures = picturesReferredTo(input.value).length;
    var files = filesReferredTo(input.value).length;
    if (pictures === 0 && files === 0) {
      pictureNote.hidden = true;
      return;
    }
    var parts = [];
    if (pictures > 0) {
      parts.push(counted(pictures, "picture"));
    }
    if (files > 0) {
      parts.push(counted(files, "file"));
    }
    pictureNote.hidden = false;
    pictureNote.textContent =
      parts.join(" and ") +
      " in the message, " +
      (pictures + files === 1 ? "where its marker is." : "each where its marker is.");
  }

  // Insert at the caret, because that is where the person was typing when they reached for the
  // picture. `setRangeText` rather than an assignment to `value`: it keeps the box's undo history
  // and leaves the caret after what it inserted, so typing carries on around the marker, and the
  // marker can be selected, cut and moved like the words it sits between.
  function putAtCaret(text) {
    input.setRangeText(text, input.selectionStart, input.selectionEnd, "end");
    input.focus();
  }

  // One at a time, in the order they were pasted: two readers finishing out of order would put the
  // second picture's marker first, and the numbers in the words are what the request is read from.
  function takeAll(files) {
    if (files.length === 0) {
      return;
    }
    take(files[0], function () {
      takeAll(files.slice(1));
    });
  }

  function isPicture(file) {
    return !!(file && file.type && file.type.indexOf("image/") === 0);
  }

  function take(file, next) {
    if (isPicture(file)) {
      takePicture(file, next);
      return;
    }
    takeFile(file, next);
  }

  function takePicture(file, next) {
    if (picturesReferredTo(input.value).length >= MAX_PICTURES) {
      say("A message carries at most " + MAX_PICTURES + " pictures.", true);
      next();
      return;
    }
    var reader = new FileReader();
    reader.onload = function () {
      if (typeof reader.result === "string") {
        var id = nextPicture;
        nextPicture += 1;
        attached.push({ id: id, url: reader.result });
        putAtCaret(markerFor(id));
        refreshNote();
      }
      next();
    };
    reader.onerror = function () {
      say("That picture could not be read.", true);
      next();
    };
    reader.readAsDataURL(file);
  }

  // The name as the person's own file system spells it, minus the characters a marker cannot hold:
  // a `]` would end the marker early, and a path separator names something that is not one file.
  // The server refuses both, so they are taken out here — where the person sees the marker that
  // results — rather than left to fail the whole message after it was typed.
  function fileNameOf(file) {
    var name = (file && file.name ? String(file.name) : "")
      .replace(/[\[\]\\/\u0000-\u001f]/g, "")
      .trim();
    if (name.length > 120) {
      name = name.slice(0, 120);
    }
    return name || "unnamed file";
  }

  function takeFile(file, next) {
    if (filesReferredTo(input.value).length >= MAX_FILES) {
      say("A message carries at most " + MAX_FILES + " files.", true);
      next();
      return;
    }
    var name = fileNameOf(file);
    for (var index = 0; index < carried.length; index += 1) {
      if (carried[index].name === name) {
        // The marker is the name, so a name can only be used once: two files sharing one would
        // send whichever was read last and silently drop the other.
        say(
          "A file named " + name + " is already in this message — rename one before sending both.",
          true
        );
        next();
        return;
      }
    }
    var reader = new FileReader();
    reader.onload = function () {
      if (typeof reader.result === "string") {
        // `readAsDataURL` gives `data:<type>;base64,<bytes>`, and the request wants the bytes: the
        // prefix is dropped here rather than carried across the wire to be dropped at the far end.
        var comma = reader.result.indexOf(",");
        if (comma > 0) {
          carried.push({ name: name, data: reader.result.slice(comma + 1) });
          putAtCaret(fileMarkerFor(name));
          refreshNote();
        }
      }
      next();
    };
    reader.onerror = function () {
      say(name + " could not be read.", true);
      next();
    };
    reader.readAsDataURL(file);
  }

  // Pasting into the panel, not into one element: a person who has just clicked somewhere in the
  // box and presses the paste key should not have to find the exact field first. Text pastes are
  // left alone — `preventDefault` runs only when the clipboard actually carries a file. Any file,
  // not only a picture: a spreadsheet copied out of a file manager arrives the same way, and the
  // version that took `image/*` and dropped the rest is the version that made a pasted csv do
  // nothing at all.
  function onPaste(event) {
    var data = event.clipboardData;
    if (!data) {
      return;
    }
    var files = [];
    var items = data.items || [];
    for (var index = 0; index < items.length; index += 1) {
      var item = items[index];
      if (item.kind === "file") {
        var file = item.getAsFile ? item.getAsFile() : null;
        if (file) {
          files.push(file);
        }
      }
    }
    if (files.length === 0) {
      return;
    }
    // A picture pasted into a textarea would otherwise be dropped by the browser silently, which
    // reads as the paste having failed.
    event.preventDefault();
    // Focus without writing anything: the caret is where the reader will insert, and an empty
    // insertion here would type over whatever the person had selected.
    input.focus();
    takeAll(files);
  }

  // `setRangeText` fires no `input` event, so a marker the person cuts out is noticed here rather
  // than at the moment it is cut.
  input.addEventListener("input", refreshNote);

  panel.addEventListener("paste", onPaste);

  // Dropping, the second way in: the clipboard is not where a downloaded file lives. The marker
  // goes in at the caret, which is where the person was typing when they reached for the file.
  panel.addEventListener("dragover", function (event) {
    if (!event.dataTransfer) {
      return;
    }
    // Without this the browser opens the dropped file instead, which loses the page the person was
    // in the middle of asking a question about.
    event.preventDefault();
    event.dataTransfer.dropEffect = "copy";
  });
  panel.addEventListener("drop", function (event) {
    var data = event.dataTransfer;
    if (!data || !data.files || data.files.length === 0) {
      return;
    }
    event.preventDefault();
    input.focus();
    takeAll(Array.prototype.slice.call(data.files));
  });

  // Choosing one, the third way in: a file that is neither on the clipboard nor being dragged
  // needs a button. The input is hidden and the button drives it so the dialog keeps its shape, and
  // the button is a real focusable one because a keyboard has to be able to reach it.
  var picker = document.createElement("input");
  picker.type = "file";
  picker.multiple = true;
  picker.id = "deepnest-dialog-picker";
  picker.hidden = true;

  var attach = document.createElement("button");
  attach.type = "button";
  attach.id = "deepnest-dialog-attach";
  attach.textContent = "Attach a file";
  attach.addEventListener("click", function () {
    picker.click();
  });
  picker.addEventListener("change", function () {
    var chosen = Array.prototype.slice.call(picker.files || []);
    // Cleared before reading: choosing the same file twice in a row fires no change event on an
    // input whose value is still that file, and the person would see the second attach do nothing.
    picker.value = "";
    if (chosen.length > 0) {
      input.focus();
      takeAll(chosen);
    }
  });

  form.appendChild(input);
  form.appendChild(attach);
  form.appendChild(picker);
  form.appendChild(send);
  form.appendChild(stop);

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
  panel.appendChild(pictureNote);
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
  //
  // This is the *final* line, so it is the one worth announcing: `aria-live` goes back on here
  // after a progress line turned it off.
  function say(text, bad) {
    answer.textContent = text;
    answer.hidden = false;
    answer.classList.remove("live");
    answer.setAttribute("aria-live", "polite");
    if (bad) {
      answer.classList.add("bad");
    } else {
      answer.classList.remove("bad");
    }
  }

  // The line under the box *while* the turn runs.
  //
  // It turns the announcement off rather than leaving it on: a region that changes every second is
  // a screen reader reading a new sentence every second, which is worse than silence. The answer
  // that matters comes through `say` above, at the end.
  function showProgress(text) {
    answer.textContent = text;
    answer.hidden = false;
    answer.classList.remove("bad");
    answer.classList.add("live");
    answer.setAttribute("aria-live", "off");
  }

  // The question route: one path, one cookie, and nothing about it needs the control token.
  function askingRoute() {
    return "/api/apps/" + encodeURIComponent(slug) + "/escalation";
  }

  // The stop route: the fourth cookie, and the only one that ends spending instead of starting it.
  function stoppingRoute() {
    return "/api/apps/" + encodeURIComponent(slug) + "/cancel";
  }

  // The conversation route: a read, and the only thing here that is.
  var TURNS_SHOWN = 20;

  function historyRoute() {
    return "/api/apps/" + encodeURIComponent(slug) + "/history";
  }

  // One turn is: what was asked, what the agent did about it, and what it said back. Rendered with
  // `textContent` throughout — every string here is either the person's words or the model's.
  // "1 picture" / "3 pictures" — the one phrase both places below need.
  function picturesIn(count) {
    return count + (count === 1 ? " picture" : " pictures");
  }

  function filesIn(count) {
    return count + (count === 1 ? " file" : " files");
  }

  // What came with the words, in the words a person uses: "with 1 picture and 2 files".
  function withWhat(pictures, files) {
    var parts = [];
    if (pictures > 0) {
      parts.push(picturesIn(pictures));
    }
    if (files > 0) {
      parts.push(filesIn(files));
    }
    return parts.join(" and ");
  }

  function renderTurn(turn, live) {
    var line = document.createElement("div");
    line.className = live ? "deepnest-turn deepnest-running" : "deepnest-turn";

    var asked = document.createElement("p");
    asked.className = "deepnest-asked";
    // A picture sent on its own is the whole message: the record has no text for it, so drawing
    // an empty line left the turn with no question at all — the reply hung under a blank row and
    // the picture itself, which the model had been looking at, appeared nowhere.
    var pictures = turn.pictures || 0;
    var files = turn.files || 0;
    // The file's contents are in the record because the model was given them; they are not what
    // the person typed, and the route reads them back as a count for exactly that reason.
    asked.textContent =
      turn.user || (pictures + files > 0 ? withWhat(pictures, files) : "");

    var did = document.createElement("p");
    did.className = "deepnest-did";
    if (turn.tools > 0) {
      did.textContent =
        "ran " + turn.tools + (turn.tools === 1 ? " tool" : " tools");
    }

    line.appendChild(asked);
    // Words and something pasted with them: the words are the question, and this says what came
    // with it. One line for both kinds, because one message carried them.
    if (turn.user && pictures + files > 0) {
      var sent = document.createElement("p");
      sent.className = "deepnest-pictures";
      sent.textContent = "with " + withWhat(pictures, files);
      line.appendChild(sent);
    }
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

  // What was last drawn, so a poll that found nothing new does not redraw: a transcript rebuilt
  // every second would fight the person scrolling through it.
  var drawn = "";

  function showTurns(turns, live) {
    if (!turns || turns.length === 0) {
      drawn = "";
      transcript.textContent = "";
      transcript.hidden = true;
      return;
    }
    var recent = turns.slice(-TURNS_SHOWN);
    var print = JSON.stringify(recent) + (live ? "+" : "");
    if (print === drawn) {
      return;
    }
    drawn = print;
    // Read the scroll position before replacing the children, and restore it only if the person was
    // already at the newest end. Auto-scrolling a reader who scrolled up is the page deciding it
    // knows better, which is the one thing a transcript being written to must not do.
    var atTheEnd =
      transcript.scrollTop + transcript.clientHeight >= transcript.scrollHeight - 8;
    transcript.textContent = "";
    recent.forEach(function (turn, index) {
      transcript.appendChild(renderTurn(turn, live && index === recent.length - 1));
    });
    transcript.hidden = false;
    if (atTheEnd) {
      // Newest at the bottom, which is where a conversation is read from.
      transcript.scrollTop = transcript.scrollHeight;
    }
  }

  function readHistory() {
    return fetch(historyRoute(), { credentials: "same-origin" })
      .then(function (response) {
        return response.ok ? response.json() : null;
      })
      .then(function (body) {
        var turns = (body && body.turns) || [];
        showTurns(turns, watching !== null);
        if (watching !== null) {
          showProgress(describeProgress(turns));
        }
      })
      .catch(function () {
        // A transcript nobody can read is not a reason to break the box that still works.
      });
  }

  // Watching a turn is reading the file again, on a timer, for as long as the request is out.
  //
  // One second, the same cadence as the question poll: a record is flushed per round and a tool
  // call takes seconds, so this cannot miss a step and is not a second copy of anything. The
  // server is not told it is being watched, and needs nothing new — a turn is legible in the file
  // the agent was always writing.
  var PROGRESS_MS = 1000;
  var watching = null;
  var sentAt = 0;

  function stopWatching() {
    if (watching !== null) {
      clearInterval(watching);
      watching = null;
    }
    // No turn in flight means nothing to stop, so the control leaves with the progress line it
    // sits under — every exit from a turn goes through here, whichever way the turn ended.
    stop.hidden = true;
    stop.disabled = false;
  }

  // "Working — round 3, 9 tool calls, 12s". The count is what separates a turn that is thinking
  // from one that is stuck, and it is the whole difference between a page that reads as
  // unresponsive and one that says what it is doing.
  function describeProgress(turns) {
    var last = turns && turns.length ? turns[turns.length - 1] : null;
    var said;
    if (!last || !last.rounds) {
      said = "Working — the model is thinking";
    } else if (last.tools > 0) {
      said =
        "Working — round " +
        last.rounds +
        ", " +
        last.tools +
        (last.tools === 1 ? " tool call" : " tool calls");
    } else {
      said = "Working — round " + last.rounds;
    }
    return said + ", " + Math.round((Date.now() - sentAt) / 1000) + "s";
  }

  function watchTheTurn() {
    sentAt = Date.now();
    stopWatching();
    // There is a turn now, and it can take minutes: this is the one moment the page can offer a
    // way out of it.
    stop.hidden = false;
    stop.disabled = false;
    // Read once immediately rather than a second from now: the first thing the person sees after
    // pressing send must not be the word the last 112 seconds were spent on.
    readHistory();
    watching = setInterval(readHistory, PROGRESS_MS);
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
    // Both timers end at the same moment, so they end in the same place: a turn that is over is no
    // longer asking a question and no longer making progress, and every exit from a turn — answer,
    // refusal, expired grant, dead server — goes through here.
    stopWatching();
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

  function ask(text, images, files) {
    // Each key is added only when there is something in it: a typed message keeps the exact body it
    // had before the box could paste anything, which is one fewer shape a server has to accept.
    var body = { message: text };
    if (images.length > 0) {
      body.images = images;
    }
    if (files.length > 0) {
      // Name and bytes, and nothing else: a file's type is what this browser guessed from the
      // extension, and the server would only be reading the guess back.
      body.files = files.map(function (entry) {
        return { name: entry.name, data: entry.data };
      });
    }
    return fetch("/api/apps/" + encodeURIComponent(slug) + "/chat", {
      method: "POST",
      // Same-origin is what attaches the cookie this page was given, and it is the default — but
      // spelled out, because the whole mechanism rests on it and a later `credentials: "omit"`
      // would turn the box into a permanent `401`.
      credentials: "same-origin",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify(body)
    });
  }

  form.addEventListener("submit", function (event) {
    event.preventDefault();
    var text = input.value.trim();
    // A picture is in the message as a marker, so a message of nothing but pictures is not empty:
    // "what is this?" is what the person means, and the marker says which picture they mean.
    if (!text) {
      return;
    }
    if (!slug) {
      // Without a slug there is no route to send to. Saying so beats posting to `/api/apps//chat`.
      say("This page does not say which app it is — open it from the control panel.", true);
      return;
    }

    send.disabled = true;
    // Two timers for the one turn: this one asks the server what the question is, and
    // `watchTheTurn` re-reads the transcript so the page can say what the turn is doing. `Asking…`
    // with nothing after it was the whole of what this page said for 112 seconds, and the reason a
    // turn that had rewritten the app read as a turn that never ran.
    watchTheTurn();
    // The turn may stop and ask a question before it answers, so the poll starts with the
    // question already possible rather than after the first answer.
    startPolling();

    ask(text, picturesReferredTo(text), filesReferredTo(text))
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
          forgetAttachments();
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

  stop.addEventListener("click", function () {
    if (!slug) {
      return;
    }
    stop.disabled = true;
    fetch(stoppingRoute(), {
      method: "POST",
      credentials: "same-origin",
      headers: { "Content-Type": "application/json" },
      body: "{}"
    })
      .then(function (response) {
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
        stop.disabled = false;
        if (result.status === 200 && result.body && typeof result.body.note === "string") {
          // The turn is not over yet — the loop reads the flag before its next request and before
          // every tool call of a batch — so this says what was asked rather than what has
          // happened. The line it replaces is the progress counter, which is still running.
          showProgress(result.body.note);
          return;
        }
        if (result.status === 401) {
          say("This page's permission to stop has expired — reload the page and try again.", true);
          return;
        }
        say("The stop did not reach deepnest (" + result.status + ").", true);
      })
      .catch(function () {
        stop.disabled = false;
        say("The stop did not reach deepnest. Is it still running?", true);
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
