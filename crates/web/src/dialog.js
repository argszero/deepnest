// The floating dialog: it stands on deepnest's own page, and the app runs in a frame beside it.
//
// The gateway serves that page rather than the agent writing the dialog into the app, so that an
// agent rewriting the app's page wholesale cannot delete it: this ball is the only way back to the
// control panel from a page the user is looking at. And the app is a frame rather than the document
// this script lives in, so that a finished rebuild — which is most turns, because a turn that
// writes a file marks the app changed — reloads the app instead of the tab: a reload cannot spare a
// subtree of the document it replaces, so the conversation has to be outside the document that
// reloads.
//
// What this file deliberately does not do:
//
// 1. **It never holds the control token.** The app's own script shares this origin, so a token
//    written into this page is a token the app can read and spend — and the control token spends
//    on every route there is.
// 2. **It calls seven routes and brings no credential of its own**: `POST /api/apps/<slug>/chat`,
//    `GET`/`POST /api/apps/<slug>/escalation` — the second is how a widening request reaches the
//    person in front of the page — `GET /api/apps/<slug>/build`, which is what a turn that changed
//    the app leaves behind to be watched, `GET /api/apps/<slug>/history`, which is what makes the
//    conversation survive a reload, `POST /api/apps/<slug>/cancel`, which stops a turn that is
//    already spending, `GET`/`POST /api/apps/<slug>/sessions`, which is the drawer, and
//    `POST /api/apps/<slug>/capture`, which is a picture of the screen. For each, the browser
//    attaches a cookie the server minted when it served this page, scoped to that one path, and all
//    seven are different values. So the box works on a page whose app is stopped or mid-rewrite,
//    and a page that cannot present that cookie gets a `401` and says so instead of failing
//    silently.
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

  // The frame the app is in, when this page is the stage. `null` is the older shape — a client that
  // sent no `Sec-Fetch-Dest`, so the gateway could not tell a top-level navigation from anything
  // else and put this script in the app's own page. Everything below works in both, and the one
  // thing that cannot is `reloadTheApp`.
  var stage = document.getElementById("deepnest-app-frame");

  var root = document.createElement("div");
  root.id = "deepnest-dialog";

  var ball = document.createElement("button");
  ball.type = "button";
  ball.id = "deepnest-dialog-ball";
  ball.setAttribute("aria-label", "deepnest");
  ball.setAttribute("aria-expanded", "false");
  ball.textContent = "d";

  // The one place on the page that says how to get here without a mouse. A silent shortcut is a
  // shortcut only the person who wrote it knows.
  //
  // The glyph is the reader's, not this file's: `Ctrl+K` on a Mac sends the wrong key, and a hint
  // that names the wrong modifier is worse than no hint. `navigator.platform` is deprecated and
  // still the only field every browser this ships in answers the same way; a browser that does not
  // answer at all falls back to the `Ctrl` spelling, which is true on everything that is not a Mac.
  var appleKeyboard = /Mac|iPhone|iPad|iPod/.test(navigator.platform || "");
  ball.setAttribute("aria-keyshortcuts", "Control+K Meta+K");
  ball.title = "deepnest — " + (appleKeyboard ? "⌘K" : "Ctrl+K");

  var panel = document.createElement("div");
  panel.id = "deepnest-dialog-panel";
  panel.hidden = true;

  var title = document.createElement("p");
  title.id = "deepnest-dialog-title";
  title.textContent = slug || "this app";

  // The way into the conversations, beside the app's name. A button rather than a link: it opens a
  // panel inside this one, and a link would navigate away from the app the person is talking about.
  var toggle = document.createElement("button");
  toggle.type = "button";
  toggle.id = "deepnest-dialog-sessions-toggle";
  toggle.textContent = "Conversations";
  toggle.setAttribute("aria-expanded", "false");
  toggle.setAttribute("aria-controls", "deepnest-dialog-sessions");

  // The way out of the small window. The panel is a floating block 22rem wide and 78vh tall at the
  // most, and an exchange does not fit in that at any window size: an answer with a code block, the
  // calls that produced it and the question after it came to 365 px on its own once the block folded
  // (#85), against the 280 px window it has. Reading a conversation a few lines at a time out of a
  // box that small is not reading it. This button gives the conversation the whole window — the
  // panel becomes the page, and the conversation keeps a reading measure inside it — and the same
  // button, saying Restore, gives the window back: a second control to undo the first would be one
  // more thing to find.
  var expand = document.createElement("button");
  expand.type = "button";
  expand.id = "deepnest-dialog-expand";
  expand.textContent = "Expand";
  expand.title = "Fill the window with this conversation, full screen (Esc)";
  expand.setAttribute("aria-pressed", "false");

  var head = document.createElement("div");
  head.id = "deepnest-dialog-head";
  head.appendChild(title);
  head.appendChild(toggle);
  head.appendChild(expand);

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

  // A stop that has been asked for and has not happened yet.
  //
  // The route answers "the turn was asked to stop", which is the truth, and what that means has
  // one half a person can rely on and one they cannot. A tool that is running is abandoned: the
  // call is told to stop and the command it spawned is killed, so the seconds a stop saves are the
  // ones a build or a search was spending. A model call in flight cannot be — an HTTP request is
  // already on the wire and there is nothing to kill — so a turn inside one ends when that call
  // returns. Both are stated rather than the better one alone, because a person who presses stop
  // during a model call and sees thirty more seconds is owed the reason.
  //
  // Measured on this page: pressing Stop during a model call changed nothing a person could see
  // for tens of seconds, because the button kept saying "Stop" and the progress line underneath it
  // went on counting. That is the reading the host reported as "pressing stop does not stop".
  //
  // So the press is said at the press. The button is the one place nothing else writes to: the
  // answer box belongs to the model's stream while it is writing, and the progress line is
  // rewritten every second by the poller below.
  var stopping = false;

  // What the control reads while a stop is in flight, and what it reads again once the turn has
  // ended. One spelling each, because a label written in two places drifts.
  var STOP_LABEL = "Stop";
  var STOPPING_LABEL = "Stopping\u2026";

  // The line under the box, and the only surface a turn that is running writes to.
  //
  // It is a reading of the machine — which round, how many calls, how long — and never the agent's
  // words. The agent's words have one home, the transcript, because a second surface for them cost
  // a hand-over to hide one of the two copies, two live regions announcing the same sentence, and a
  // box that had to be both "what the model is saying" and "what the machine is doing" — which is
  // what its three state classes were there to tell apart. All of that was machinery repairing the
  // first mistake, which was having a second surface at all.
  //
  // `aria-live` is off while it counts: a region rewritten every second is a screen reader reading a
  // new sentence every second. The one thing said here that is worth hearing — a stop that was asked
  // for — turns it back on, once.
  var status = document.createElement("div");
  status.id = "deepnest-dialog-status";
  status.hidden = true;
  status.setAttribute("role", "status");
  status.setAttribute("aria-live", "off");

  // What this page itself has to say, and what the turn cost.
  //
  // In a conversation these are rows, not a box under the input: a failed send, a rebuild that did
  // not work, a grant that expired, a stop that was heard. Held here rather than written straight
  // into the transcript's DOM because the transcript is redrawn from the file every second, and a
  // row only this page knows about would go with the next redraw.
  var notices = [];
  var spendLine = "";

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

  // The one place a picture joins the message, whichever way it arrived: the clipboard, the file
  // picker, or the screenshot. More than one way in and one place to land is what keeps the marker,
  // the number and the limit from disagreeing between them — and the limit is checked here, where
  // every way in arrives, so a caller written later cannot bypass it.
  function attachPicture(url) {
    if (picturesReferredTo(input.value).length >= MAX_PICTURES) {
      say("A message carries at most " + MAX_PICTURES + " pictures.", true);
      return false;
    }
    var id = nextPicture;
    nextPicture += 1;
    attached.push({ id: id, url: url });
    putAtCaret(markerFor(id));
    refreshNote();
    return true;
  }

  function takePicture(file, next) {
    // The same limit, asked before the file is read rather than after: reading eight pictures to
    // throw them away is work done for a message that is already full, and `next` has to be called
    // either way. `attachPicture` asks again because it is the gate the screenshot path reaches.
    if (picturesReferredTo(input.value).length >= MAX_PICTURES) {
      say("A message carries at most " + MAX_PICTURES + " pictures.", true);
      next();
      return;
    }
    var reader = new FileReader();
    reader.onload = function () {
      if (typeof reader.result === "string") {
        attachPicture(reader.result);
      }
      next();
    };
    reader.onerror = function () {
      say("That picture could not be read.", true);
      next();
    };
    reader.readAsDataURL(file);
  }

  // ---- The screenshot, from the press to the attachment ----
  //
  // The panel sits over the screen, so a picture taken while it is up is a picture of the panel. It
  // goes away first — and the wait below is two animation frames, not one: the first hands the
  // change to the compositor and the second runs after the frame carrying it has been painted. One
  // frame was measured to be enough on a warm tab and not on one that had just loaded.
  function afterPaint(next) {
    if (document.hidden || typeof window.requestAnimationFrame !== "function") {
      // Nothing is being painted, so there is no frame to wait for; the delay is what still puts
      // the panel's disappearance in front of the capture.
      window.setTimeout(next, 60);
      return;
    }
    window.requestAnimationFrame(function () {
      window.requestAnimationFrame(next);
    });
  }

  function startCapture() {
    panelWasOpen = !panel.hidden;
    panel.hidden = true;
    ball.hidden = true;
    afterPaint(function () {
      fetch(routeFor("capture"), { method: "POST", credentials: "same-origin" })
        .then(function (response) {
          if (response.status === 401) {
            // The grant is gone: expired, or this page was not served by this process. Reloading
            // is the one action that mints another — the same reading as on the box.
            throw new Error(
              "This page's permission to take a picture has expired — reload the page."
            );
          }
          return response.json().then(function (answer) {
            if (!response.ok) {
              // The server's sentence is the whole answer: "screencapture exited 1: …" says what
              // "the screenshot failed" cannot, and it is what tells a person to grant the
              // permission it names. Shown through `textContent`, never as markup.
              throw new Error((answer && answer.error) || "The screen could not be captured.");
            }
            return answer.image;
          });
        })
        .then(function (image) {
          if (typeof image !== "string" || image.indexOf("data:image/") !== 0) {
            throw new Error("The screen was captured but no picture arrived.");
          }
          openShot(image);
        })
        .catch(function (error) {
          restorePanel();
          say(error && error.message ? error.message : "The screen could not be captured.", true);
        });
    });
  }

  function restorePanel() {
    ball.hidden = false;
    // Through `show` rather than by assigning `hidden`: opening is also when the transcript is
    // asked for again, and the panel comes back the way it was found rather than always open.
    show(panelWasOpen);
  }

  function openShot(image) {
    shotFrame.src = image;
    shotBox.hidden = true;
    dragFrom = null;
    selection = null;
    shotUse.disabled = true;
    shotHint.textContent = "Drag over the part you mean.";
    shot.hidden = false;
    // The sheet takes the keyboard so Escape and Enter are answered here rather than by the page
    // underneath, which is covered by it.
    shot.focus();
  }

  function closeShot() {
    shot.hidden = true;
    shotFrame.removeAttribute("src");
    shotBox.hidden = true;
    dragFrom = null;
    selection = null;
    shotUse.disabled = true;
  }

  // The picture as it is actually drawn. `object-fit: contain` letterboxes it, so the element's box
  // and the picture's box are two different rectangles — and only the second one maps to pixels.
  function drawnFrame() {
    var box = shotFrame.getBoundingClientRect();
    var wide = shotFrame.naturalWidth;
    var tall = shotFrame.naturalHeight;
    if (!wide || !tall || !box.width || !box.height) {
      return null;
    }
    var scale = Math.min(box.width / wide, box.height / tall);
    var width = wide * scale;
    var height = tall * scale;
    return {
      left: box.left + (box.width - width) / 2,
      top: box.top + (box.height - height) / 2,
      width: width,
      height: height,
      scale: scale
    };
  }

  function clampToFrame(x, y, drawn) {
    return {
      x: Math.min(Math.max(x, drawn.left), drawn.left + drawn.width),
      y: Math.min(Math.max(y, drawn.top), drawn.top + drawn.height)
    };
  }

  function drawSelection() {
    if (!selection) {
      shotBox.hidden = true;
      shotUse.disabled = true;
      return;
    }
    shotBox.hidden = false;
    shotBox.style.left = selection.left + "px";
    shotBox.style.top = selection.top + "px";
    shotBox.style.width = selection.width + "px";
    shotBox.style.height = selection.height + "px";
    shotUse.disabled = false;
  }

  function useSelection() {
    var drawn = drawnFrame();
    if (!drawn || !selection) {
      return;
    }
    // Back to picture pixels before cropping: the selection is in viewport pixels, and those shrink
    // the picture by `drawn.scale` before it is ever drawn.
    var url = croppedPicture(
      (selection.left - drawn.left) / drawn.scale,
      (selection.top - drawn.top) / drawn.scale,
      selection.width / drawn.scale,
      selection.height / drawn.scale
    );
    closeShot();
    restorePanel();
    if (url) {
      attachPicture(url);
    }
  }

  function useWholeFrame() {
    if (!drawnFrame()) {
      return;
    }
    var url = croppedPicture(0, 0, shotFrame.naturalWidth, shotFrame.naturalHeight);
    closeShot();
    restorePanel();
    if (url) {
      attachPicture(url);
    }
  }

  // The region, scaled so its long edge fits `CAPTURE_LONGEST` and never scaled up: a crop of a
  // small region is already the size to send it at, and a whole 4K screen is 8.3 megapixels, whose
  // `data:` URL would be several times the largest message this server accepts.
  function croppedPicture(left, top, width, height) {
    var scale = Math.min(1, CAPTURE_LONGEST / Math.max(width, height));
    var canvas = document.createElement("canvas");
    canvas.width = Math.max(1, Math.round(width * scale));
    canvas.height = Math.max(1, Math.round(height * scale));
    var context = canvas.getContext("2d");
    if (!context) {
      return null;
    }
    context.drawImage(shotFrame, left, top, width, height, 0, 0, canvas.width, canvas.height);
    return canvas.toDataURL("image/png");
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

  // The screenshot. A sibling of "Attach a file" rather than a mode: a person who wants to point at
  // something on their screen reaches for it exactly where they reach for a file, and the picture it
  // produces lands in the message the same way.
  //
  // The picture is taken by the deepnest process, not by this page. The browser's own screen-share
  // API is what a page would reach for, and the price is a picker for every press plus a permission
  // prompt this page cannot answer for the person in front of it — and the dialogs that costs are
  // the whole reason the request goes to the server instead. What comes back is a `data:` URL of the
  // whole screen, and the sheet below is where a person says which part of it they meant.
  var CAPTURE_LONGEST = 1600;

  var capture = document.createElement("button");
  capture.type = "button";
  capture.id = "deepnest-dialog-capture";
  capture.textContent = "Screenshot";
  capture.addEventListener("click", startCapture);

  // The frozen frame, and the rectangle dragged over it. Full-viewport and outside the panel on
  // purpose: the panel hides itself for the capture (a picture of the panel is the one thing this
  // must not produce), so the sheet cannot be a child of it.
  var shot = document.createElement("div");
  shot.id = "deepnest-dialog-shot";
  shot.hidden = true;
  shot.tabIndex = -1;
  shot.setAttribute("role", "dialog");
  shot.setAttribute("aria-modal", "true");
  shot.setAttribute("aria-label", "Choose the part of the screen to send");

  var shotFrame = document.createElement("img");
  shotFrame.id = "deepnest-dialog-shot-frame";
  // The frame is the person's own screen, and a screen reader announcing it would be reading out a
  // picture of whatever happens to be behind this page.
  shotFrame.alt = "";

  var shotBox = document.createElement("div");
  shotBox.id = "deepnest-dialog-shot-box";
  shotBox.hidden = true;

  var shotBar = document.createElement("div");
  shotBar.id = "deepnest-dialog-shot-bar";

  var shotHint = document.createElement("p");
  shotHint.id = "deepnest-dialog-shot-hint";
  shotHint.textContent = "Drag over the part you mean.";

  var shotUse = document.createElement("button");
  shotUse.type = "button";
  shotUse.id = "deepnest-dialog-shot-use";
  shotUse.textContent = "Use this part";
  shotUse.disabled = true;
  shotUse.addEventListener("click", useSelection);

  var shotWhole = document.createElement("button");
  shotWhole.type = "button";
  shotWhole.id = "deepnest-dialog-shot-whole";
  shotWhole.textContent = "The whole screen";
  shotWhole.addEventListener("click", useWholeFrame);

  var shotCancel = document.createElement("button");
  shotCancel.type = "button";
  shotCancel.id = "deepnest-dialog-shot-cancel";
  shotCancel.textContent = "Cancel";
  shotCancel.addEventListener("click", function () {
    closeShot(true);
  });

  shotBar.appendChild(shotHint);
  shotBar.appendChild(shotUse);
  shotBar.appendChild(shotWhole);
  shotBar.appendChild(shotCancel);
  shot.appendChild(shotFrame);
  shot.appendChild(shotBox);
  shot.appendChild(shotBar);

  // Which half of the panel was open when the capture started. The panel goes away so it is not in
  // its own picture, and this is what puts it back the way it was found.
  var panelWasOpen = false;
  // Where the drag started, and what it currently covers, both in viewport pixels.
  var dragFrom = null;
  var selection = null;

  // Bound here rather than beside the functions they call: a registration is a statement and runs
  // where it sits, and every name it uses is either declared above or a function declaration, which
  // hoists. Bound further up they would have been attached to `undefined` — the `var` that makes
  // the sheet hoists its name and not its value.
  shot.addEventListener("pointerdown", function (event) {
    // A press on the bar is a press on a button, not the start of a region. Without this the drag
    // would take it: the press would set an empty selection, the release would clear it and disable
    // "Use this part" — and the button would be disabled by the very event that was about to press
    // it. Measured in Chrome, where the first version of this handler did exactly that.
    if (shotBar.contains(event.target)) {
      return;
    }
    var drawn = drawnFrame();
    if (!drawn) {
      return;
    }
    var point = clampToFrame(event.clientX, event.clientY, drawn);
    dragFrom = point;
    selection = { left: point.x, top: point.y, width: 0, height: 0 };
    drawSelection();
    if (shot.setPointerCapture) {
      shot.setPointerCapture(event.pointerId);
    }
    event.preventDefault();
  });

  shot.addEventListener("pointermove", function (event) {
    var drawn = drawnFrame();
    if (!drawn || !dragFrom) {
      return;
    }
    var point = clampToFrame(event.clientX, event.clientY, drawn);
    selection = {
      left: Math.min(dragFrom.x, point.x),
      top: Math.min(dragFrom.y, point.y),
      width: Math.abs(point.x - dragFrom.x),
      height: Math.abs(point.y - dragFrom.y)
    };
    drawSelection();
  });

  shot.addEventListener("pointerup", function (event) {
    // The release of a press that never became a drag: a click on the bar, or a press that landed
    // before the frame was drawn. There is no region to finish.
    if (!dragFrom) {
      return;
    }
    dragFrom = null;
    // A press with no drag is not a region. Deselecting is the reading a person expects from a
    // stray click, and sending a four-pixel picture would be the one they do not.
    if (selection && (selection.width < 4 || selection.height < 4)) {
      selection = null;
      drawSelection();
    }
    event.preventDefault();
  });

  shot.addEventListener("keydown", function (event) {
    // Answered here and stopped, because the page underneath has its own Escape (which closes the
    // panel) and its own Enter. Without this, one Escape would do two things at once.
    if (event.key === "Escape") {
      event.preventDefault();
      event.stopPropagation();
      closeShot();
      restorePanel();
      return;
    }
    if (event.key === "Enter" && selection) {
      event.preventDefault();
      event.stopPropagation();
      useSelection();
    }
  });

  form.appendChild(input);
  form.appendChild(attach);
  form.appendChild(capture);
  form.appendChild(picker);
  form.appendChild(send);
  form.appendChild(stop);

  var link = document.createElement("a");
  link.id = "deepnest-dialog-link";
  link.href = "/";
  link.textContent = "Open the control panel";
  // In the head, beside the other ways out of this panel, rather than under the box a person types
  // in: the input is the bottom edge and what is under it is the status line alone.
  head.appendChild(link);

  // The conversation, read back from the server rather than remembered here. That is what makes
  // it survive the reload a finished rebuild performs: the page comes back empty and asks again,
  // and the answer it was showing is in the file the agent wrote, not in a variable that died.
  var transcript = document.createElement("div");
  transcript.id = "deepnest-dialog-transcript";
  transcript.setAttribute("role", "log");
  transcript.setAttribute("aria-label", "This app's conversation");
  // Reachable by Tab, and that is the whole point: this is the one box in the dialog that scrolls
  // on its own, and a box a keyboard cannot focus is a box a keyboard cannot scroll. Measured in
  // Chrome with real key presses before this line existed: the tab order ran from the ball to
  // "Conversations", then through the twelve copy buttons *inside* the transcript, then to the jump
  // button and the message box — the transcript itself never took focus, so the answer above the
  // fold could be read with a mouse and not with a keyboard. `role="log"` and its label were
  // already here; what was missing was a way in.
  transcript.setAttribute("tabindex", "0");

  // Finding an exchange, in a conversation too long to read through.
  //
  // The transcript draws the newest twenty turns and offers a press for the twenty before them, so
  // in a long conversation most of it is not in the page at all: measured in Chrome over forty
  // exchanges, the panel opened with twenty drawn, the first exchange on screen was number 21, and
  // the browser's own find for a phrase in exchange 7 answered `false` — a page cannot be searched
  // for words it does not contain. Nine presses is what it took to have two hundred exchanges in
  // one conversation on screen.
  //
  // So the field appears only when the panel is withholding something: on a conversation that fits
  // in the window there is nothing for it to find that scrolling would not, and a search box over
  // three exchanges is furniture. While it holds a phrase the window is not applied at all and the
  // search runs over every exchange the conversation has.
  var find = document.createElement("input");
  find.type = "search";
  find.id = "deepnest-dialog-find";
  find.placeholder = "Find an exchange…";
  find.setAttribute("aria-label", "Find an exchange in this conversation");
  find.hidden = true;

  var found = document.createElement("p");
  found.id = "deepnest-dialog-found";
  found.hidden = true;
  found.setAttribute("role", "status");

  // The way back to the newest line, offered only when the reader has left it. A transcript that
  // being written to keeps pulling its own scroll to the end is a transcript a person cannot read
  // while the agent works, which is exactly when reading it is worth something.
  var jump = document.createElement("button");
  jump.type = "button";
  jump.id = "deepnest-dialog-jump";
  jump.textContent = "Jump to the latest";
  jump.hidden = true;

  // The drawer: every conversation this app has, and the way to start another one. It sits above
  // the transcript rather than replacing it, so a person can see what they are leaving before they
  // leave it — the row they are on is marked, and the row they are going to is one line above it.
  var drawer = document.createElement("section");
  drawer.id = "deepnest-dialog-sessions";
  drawer.hidden = true;
  drawer.setAttribute("aria-label", "This app's conversations");

  var drawerList = document.createElement("div");
  drawerList.id = "deepnest-dialog-sessions-list";

  var startOne = document.createElement("button");
  startOne.type = "button";
  startOne.id = "deepnest-dialog-sessions-new";
  startOne.textContent = "Start a new conversation";

  drawer.appendChild(drawerList);
  drawer.appendChild(startOne);

  panel.appendChild(head);
  panel.appendChild(note);
  panel.appendChild(drawer);
  panel.appendChild(find);
  panel.appendChild(found);
  panel.appendChild(transcript);
  panel.appendChild(jump);
  panel.appendChild(pictureNote);
  // The widening request sits in the composer area, above the box it is about to widen: it is a
  // question about the next call, so it belongs with the next message rather than under it.
  panel.appendChild(asked);
  panel.appendChild(form);
  // And under the box, one line: what the turn is doing. Nothing else. The input is the panel's
  // bottom edge, and every word this dialog has to show is a row in the transcript above it.
  panel.appendChild(status);
  root.appendChild(panel);
  root.appendChild(ball);
  // Last, and outside the panel: it covers the viewport, and it is shown at the one moment the
  // panel is not.
  root.appendChild(shot);
  host.appendChild(root);

  // Kept across a close and a reopen. The person chose a bigger window, and making them choose it
  // again every time the panel is summoned would hand the small one back as a surprise.
  var expanded = false;

  // The state goes on the panel rather than on a class of the root, because the root is positioned
  // by the page's own corner and holds the ball too: what changes size is this box.
  function setExpanded(next) {
    expanded = next;
    panel.setAttribute("data-expanded", next ? "yes" : "no");
    expand.setAttribute("aria-pressed", next ? "true" : "false");
    expand.textContent = next ? "Restore" : "Expand";
    expand.title = next
      ? "Back to the small window (Esc)"
      : "Fill the window with this conversation, full screen (Esc)";
    holdThePage();
  }

  // A full-screen sheet is the page for as long as it is up, so the page underneath stops moving:
  // a scroll wheel over a transcript that has reached its end would otherwise walk the app somewhere
  // nobody can see. It is also what makes the sheet reach the edges — a classic scrollbar takes 15 px
  // of the layout viewport on every Windows build, and that strip of app showing down the right-hand
  // edge is what this was measured on.
  //
  // The lock is on the document element and only while the sheet is actually on screen: an expanded
  // panel that has been closed leaves the page scrollable again, which is why this is called from
  // both places that change either half rather than from `setExpanded` alone.
  function holdThePage() {
    var holding = expanded && !panel.hidden;
    if (holding) {
      document.documentElement.classList.add("deepnest-dialog-locked");
    } else {
      document.documentElement.classList.remove("deepnest-dialog-locked");
    }
    // And the app, when it is a frame: that document is the one with something to scroll, and the
    // class above reaches only this one — the app's page does not load this stylesheet, so the
    // hold is written where the scrolling happens instead of named where it does not.
    if (stage !== null) {
      try {
        var app = stage.contentDocument;
        if (app && app.documentElement) {
          app.documentElement.style.overflow = holding ? "hidden" : "";
        }
      } catch (error) {
        // A frame this document cannot reach. Nothing is being held, which is the state a panel
        // that is not expanded wants anyway.
      }
    }
  }

  expand.addEventListener("click", function () {
    setExpanded(!expanded);
  });

  function show(open) {
    panel.hidden = !open;
    ball.setAttribute("aria-expanded", open ? "true" : "false");
    holdThePage();
    if (open) {
      // Read before focus: the transcript is what the person came to see, and asking for it here
      // is also what makes the page's first open after a rebuild show the turn that caused it.
      readHistory();
      input.focus();
    }
  }

  // What this page says when it has something to say.
  //
  // A row in the conversation rather than a line under the input. The input is the panel's bottom
  // edge, and everything above it is one scrolling record of what happened — the shape every chat
  // window has, and the reason it has it: a second place for words is a second place to keep in
  // step, to hide when it goes stale, and to explain to a reader who cannot see either.
  function say(text, bad) {
    notices.push({ text: String(text), bad: !!bad });
    drawNow();
  }

  // The line under the box.
  //
  // A stop that has been asked for takes it over, because the counter is a claim that work is
  // progressing and once the person has ended it that claim is the wrong one: the turn is finishing
  // what it was in the middle of, not getting further. That is the one thing said here worth
  // hearing, so it is the one thing announced — once, unlike a counter that rewrites itself.
  function showStopping(text) {
    status.textContent = text;
    status.hidden = false;
    status.setAttribute("aria-live", "polite");
  }

  function showProgress(text) {
    if (stopping) {
      return;
    }
    status.textContent = text;
    status.hidden = false;
    status.setAttribute("aria-live", "off");
  }

  // Nothing is running, so the line goes: a status left behind is a claim about a turn that ended.
  function clearStatus() {
    status.textContent = "";
    status.hidden = true;
    status.setAttribute("aria-live", "off");
  }

  // The turns as the last read of the file found them, and whether that read was taken mid-turn.
  // Kept so a redraw this page starts on its own — a notice, a piece of the answer still being
  // written — does not have to ask the server a second time to draw what it already has.
  var read = [];
  var isLive = false;

  function drawNow() {
    showTurns(read, isLive);
  }

  // The answer as the model writes it, rather than in one piece when the turn is over.
  //
  // A turn can take minutes and most of that is tools, but the last part is the model writing its
  // answer — and until now the page had nothing to show for it until the whole body arrived, so the
  // end of a long turn looked exactly like a turn that had hung. The chat route has answered as an
  // event stream since it was written; this is the reader.
  //
  // It answers two ways and both are this page's job: the stream when the page asks for one, and
  // one JSON body for everything that is not an answer — a refusal, an expired grant, a process
  // with no model configured. `Accept` is the whole of what asks, and the reply's own content type
  // is what decides the reading, so a refusal is never mistaken for a stream that ended early.
  var STREAM = "text/event-stream";

  function wantsEvents(response) {
    return (response.headers.get("content-type") || "").indexOf(STREAM) !== -1;
  }

  // One event: `event: <kind>` and `data: <json>`, and a blank line to end it. The kinds this route
  // writes are `text` for a piece of the answer, then exactly one of `done` or `error` carrying the
  // reply the JSON path hands over whole.
  function readEvent(block) {
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
      return null;
    }
    try {
      return { kind: kind, body: JSON.parse(data) };
    } catch (error) {
      // A frame nobody can parse is not worth a line of its own: the turn's own `done`, or the
      // connection ending, is what says how this went.
      return null;
    }
  }

  // Read the stream to its end, handing every piece of the answer to `onText` the moment it exists,
  // and resolve with the reply the closing event carried.
  function readEvents(response, onText) {
    var reader = response.body.getReader();
    var decoder = new TextDecoder();
    var buffered = "";
    var reply = null;

    function take(block) {
      var event = readEvent(block);
      if (event === null) {
        return;
      }
      if (event.kind === "text" && event.body && typeof event.body.text === "string") {
        onText(event.body.text);
      } else if (event.kind === "done" || event.kind === "error") {
        reply = event.body;
      }
    }

    function pump() {
      return reader.read().then(function (step) {
        if (step.done) {
          // What is left is a frame the server never finished writing. It is dropped rather than
          // half-read: half an answer shown as an answer is worse than none, and the caller has the
          // missing reply to go on.
          return reply;
        }
        // `stream: true` because a chunk may end in the middle of a multi-byte character, and a
        // decoder that flushed per chunk would put a replacement character in the answer.
        buffered += decoder.decode(step.value, { stream: true });
        var cut = buffered.indexOf("\n\n");
        while (cut !== -1) {
          take(buffered.slice(0, cut));
          buffered = buffered.slice(cut + 2);
          cut = buffered.indexOf("\n\n");
        }
        return pump();
      });
    }

    return pump();
  }

  // The text in flight: the one round the model is writing right now, and nothing else.
  //
  // Every round of a turn is recorded as it ends — that is what a turn's `said` is — and the
  // transcript draws every recorded round where it happened. So the only words the record does not
  // have are the ones being written this second, and these are exactly those. Nothing is
  // accumulated across rounds any more: that accumulation is what put round 1, 3 and 5 into one
  // growing line under the input, out of order and beside a transcript already drawing them.
  var writing = "";
  var writingNode = null;
  // How many rounds the running turn had when this page last looked, or null until it has looked
  // once. A round is recorded the moment it ends, so a round count that has moved means the words
  // being held here are in the file now and must not be drawn a second time.
  var writingBase = null;
  var writingTask = 0;

  // A piece of the answer, drawn where it belongs: at the end of the running turn, which is where
  // the recorded round will take its place when this one ends.
  //
  // Drawn through the transcript's own renderer rather than shown as text, because the answer is
  // Markdown: measured in Chrome against the old line, a person watching an answer arrive read
  // `##`, `**` and a pair of fences as literal characters, while the same words one redraw later
  // were headings, bold runs and a code block.
  function typeMore(piece) {
    writing += piece;
    // Coalesced to one draw per task rather than one per piece: a stream arrives in hundreds of
    // pieces and each draw rebuilds this block, so drawing per piece would rebuild it a quadratic
    // number of times. A trailing draw is what a reader sees — the last piece always lands, which
    // is the difference between coalescing and dropping.
    if (writingTask) {
      return;
    }
    writingTask = setTimeout(function () {
      writingTask = 0;
      if (!writing) {
        // The turn ended while the words were waiting to be drawn. There is nothing to draw: the
        // round they belonged to is in the file, and the record draws it.
        return;
      }
      if (writingNode === null) {
        // The first piece can arrive before the read of the file that finds this turn, so there is
        // nowhere yet to put it: one redraw is what makes the turn, and this line, exist.
        drawNow();
      } else {
        fillWriting();
      }
    }, 0);
  }

  function fillWriting() {
    if (writingNode === null) {
      return;
    }
    // Read before the words are replaced, the way the transcript reads its own scroll: a person who
    // scrolled up to read something is not to be pulled back down by the next token.
    var following = atTheLatest();
    writingNode.textContent = "";
    var said = document.createElement("div");
    said.className = "deepnest-said";
    // `null` is "this text is being shown, not copied from": a copy control over a sentence still
    // growing would copy half of it, and the transcript offers one for the round once it is whole.
    said.appendChild(renderWords(writing, null));
    writingNode.appendChild(said);
    if (following) {
      transcript.scrollTop = transcript.scrollHeight;
    }
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

  // How many of them are drawn now. The server sends every turn the app has — it has no reason not
  // to, the file is the record — so a transcript drawing a fixed last twenty was not a view of the
  // conversation, it was the conversation with its beginning silently gone: a session long enough
  // and the first exchange, which is the one that says what the app was asked to be, could not be
  // read back at all. This is how far back the window reaches, and it only ever grows.
  var shownTurns = TURNS_SHOWN;

  // Set when the next redraw is adding turns *above* the ones already drawn. The reader's place is
  // then the same line seen from below, and it has moved down by exactly what was added above it.
  var holdingPlace = false;

  function historyRoute() {
    return "/api/apps/" + encodeURIComponent(slug) + "/history";
  }

  function sessionsRoute() {
    return "/api/apps/" + encodeURIComponent(slug) + "/sessions";
  }

  // When a conversation was last used, as a row in a list can show it: the day and the minute, and
  // nothing finer. The server sends the file's own clock, so this prints rather than decides.
  // A time as a person reads a clock, not as a file records one.
  //
  // The record stores UTC with a trailing `Z`, and this page used to print its digits unchanged —
  // so measured at UTC+8, a conversation held at 15:10 was listed as `07:10`, and an exchange asked
  // at 23:40Z was dated to the *previous day*. The panel is read by the person who was there, in the
  // zone they were in, so the conversion is the browser's job and the record keeps its own precision
  // in the `datetime` attribute.
  var MONTHS = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun",
    "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
  ];

  function padded(value) {
    return value < 10 ? "0" + value : String(value);
  }

  function instantOf(at) {
    if (!at || at.length < 16) {
      return null;
    }
    var instant = new Date(at);
    // An unreadable record is a record with no time, not a page that draws `NaN`.
    return isNaN(instant.getTime()) ? null : instant;
  }

  // `15:10` — the reading a transcript repeats, where the day is already the reader's today.
  function clockOf(instant) {
    return padded(instant.getHours()) + ":" + padded(instant.getMinutes());
  }

  // `2 Nov`, with the year only when it is not this one: a conversation from last year read as
  // `2 Nov` would be a conversation dated wrongly.
  function dayOf(instant) {
    var day =
      instant.getDate() +
      " " +
      MONTHS[instant.getMonth()];
    return instant.getFullYear() === new Date().getFullYear()
      ? day
      : day + " " + instant.getFullYear();
  }

  function sameLocalDay(left, right) {
    return (
      left.getFullYear() === right.getFullYear() &&
      left.getMonth() === right.getMonth() &&
      left.getDate() === right.getDate()
    );
  }

  // The whole stamp, `2026-11-02 15:10`, for the places that name a conversation rather than repeat
  // it — the drawer already spends a line on how many exchanges there are.
  function whenOf(at) {
    var instant = instantOf(at);
    if (instant === null) {
      return "";
    }
    return (
      instant.getFullYear() +
      "-" +
      padded(instant.getMonth() + 1) +
      "-" +
      padded(instant.getDate()) +
      " " +
      clockOf(instant)
    );
  }

  // One row per conversation. Built as nodes, never as markup: a conversation's title is the
  // person's own first sentence, and this page does not put anybody's words into `innerHTML`.
  function showSessions(body) {
    var sessions = (body && body.sessions) || [];
    var current = (body && body.current) || "";
    drawerList.textContent = "";
    sessions.forEach(function (session) {
      var row = document.createElement("button");
      row.type = "button";
      row.className = session.current
        ? "deepnest-session deepnest-session-current"
        : "deepnest-session";
      // Told, not only drawn: a reader who cannot see the mark still has to know which
      // conversation their next message would land in.
      row.setAttribute("aria-current", session.current ? "true" : "false");
      var said = document.createElement("span");
      said.className = "deepnest-session-title";
      said.textContent = session.title || "Nothing asked yet";
      var meta = document.createElement("span");
      meta.className = "deepnest-session-meta";
      var when = whenOf(session.at);
      meta.textContent =
        counted(session.turns, "exchange") + (when ? " · " + when : "");
      row.appendChild(said);
      row.appendChild(meta);
      row.addEventListener("click", function () {
        if (session.current) {
          setDrawer(false);
          return;
        }
        chooseSession(session.id);
      });
      drawerList.appendChild(row);
    });
    if (sessions.length === 0) {
      var nothing = document.createElement("p");
      nothing.className = "deepnest-session-none";
      nothing.textContent = "No conversations yet.";
      drawerList.appendChild(nothing);
    }
  }

  function setDrawer(open) {
    drawer.hidden = !open;
    toggle.setAttribute("aria-expanded", open ? "true" : "false");
    if (open) {
      readSessions();
    }
  }

  function readSessions() {
    return fetch(sessionsRoute(), { credentials: "same-origin" })
      .then(function (response) {
        return response.ok ? response.json() : null;
      })
      .then(function (body) {
        if (body) {
          showSessions(body);
        }
      })
      .catch(function () {
        say("The conversations could not be listed — reload the page.", true);
      });
  }

  // One call for both choices: "open this one" and "start another" differ by the body alone, and
  // the answer to either is the list the drawer draws, so the two cannot render differently.
  function choose(body, onDone) {
    return fetch(sessionsRoute(), {
      method: "POST",
      credentials: "same-origin",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify(body)
    })
      .then(function (response) {
        return response
          .json()
          .catch(function () {
            return null;
          })
          .then(function (answer) {
            if (!response.ok) {
              // A `409` is the interesting one: a turn is running, and the answer says so in the
              // words the server wrote rather than in a status code the page would have to explain.
              say(
                (answer && answer.error) ||
                  "That conversation could not be opened — reload the page.",
                true
              );
              return null;
            }
            return answer;
          });
      })
      .catch(function () {
        say("That conversation could not be opened — reload the page.", true);
        return null;
      });
  }

  function chooseSession(id) {
    return choose({ open: id }).then(function (body) {
      if (body) {
        adopt(body);
      }
    });
  }

  function startSession() {
    return choose({ new: true }).then(function (body) {
      if (body) {
        adopt(body);
      }
    });
  }

  // Moving the page onto another conversation. The transcript is emptied and read again rather
  // than edited: what is drawn there belongs to the session being left, and a transcript that
  // grew a second conversation's lines under the first is a page telling two stories at once.
  function adopt(body) {
    showSessions(body);
    setDrawer(false);
    drawn = "";
    shownTurns = TURNS_SHOWN;
    holdingPlace = false;
    transcript.textContent = "";
    transcript.hidden = true;
    transcript.scrollTop = 0;
    say("This is now the conversation the agent writes to.");
    readHistory();
    input.focus();
  }

  toggle.addEventListener("click", function () {
    setDrawer(drawer.hidden);
  });

  startOne.addEventListener("click", startSession);

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

  // ---- The calls a turn made ---------------------------------------------------------------

  // A disclosure a person opened stays open across a poll. The transcript is rebuilt whenever the
  // file grew, which while a turn runs is every round, and a row that shut itself while its own
  // turn was still running would be a row nobody could read.
  var openedCalls = {};

  // How a call ended, in the one word the row shows.
  //
  // `live` is what separates a call that is still out from one that never answered: the same empty
  // state means "running" in the turn being watched and "no result" in a turn that is over.
  function stateWord(state, live) {
    if (state === "ok" || state === "failed" || state === "refused") {
      return state;
    }
    // An ending the file recorded before it recorded which one. Not "ok": nothing said it was.
    if (state === "done") {
      return "done";
    }
    return live ? "running" : "no result";
  }

  // The batches, and whether one is open — kept apart from the calls' own disclosures because a
  // group and a call inside it are two different folds, both of which have to survive a poll.
  var openedGroups = {};

  // The turn's body, in the order it happened: what the agent said in a round, the calls that
  // round asked for, the next round's words, and so on.
  //
  // This is what the record always held and the transcript did not show. A model that says why it
  // is about to do something, does it, and says what it found before doing the next thing leaves a
  // turn whose shape is "words, batch, words, batch" — and the page drew every call of the turn as
  // one flat list with the answer under all of them, so the remark that separated two batches was
  // lost with it. The host reading the transcript named the rule on 2026-11-02T21:14: every
  // message separates one batch of calls from the next.
  //
  // `null` when the turn has neither words nor calls, so the caller draws nothing at all.
  // The agent's own note: why a turn ended early, what it could not run. It is the line a person
  // needed and could not see — a turn that spent its budget on tools has no answer, and without
  // the note it is indistinguishable from a turn that never ran — so it keeps its own weight, and
  // it is drawn where it happened rather than collected at the end of the turn.
  function renderNote(text) {
    var noteLine = document.createElement("p");
    noteLine.className = "deepnest-note";
    noteLine.textContent = text;
    return noteLine;
  }

  function renderStream(turn, live) {
    var calls = turn.calls || [];
    var byRound = {};
    (turn.said || []).forEach(function (line) {
      var round = typeof line.round === "number" ? line.round : 0;
      if (!byRound[round]) {
        byRound[round] = [];
      }
      // A round that wrote twice (a message and its continuation) keeps its order.
      byRound[round].push(String(line.text || ""));
    });
    var numbered = {};
    Object.keys(byRound).forEach(function (round) {
      numbered[round] = true;
    });
    calls.forEach(function (call) {
      numbered[typeof call.round === "number" ? call.round : 0] = true;
    });
    // A note is something the agent wrote down at a moment in the turn, and the moment is what
    // places it. Without this a compaction — written before the turn's first request — was drawn
    // under every call the turn went on to make, and read as the last thing that happened.
    (turn.notes || []).forEach(function (note) {
      numbered[typeof note.round === "number" ? note.round : 0] = true;
    });
    var order = Object.keys(numbered)
      .map(Number)
      .sort(function (a, b) {
        return a - b;
      });
    if (order.length === 0) {
      return null;
    }
    var box = document.createElement("div");
    box.className = "deepnest-stream";
    order.forEach(function (round) {
      (byRound[round] || []).forEach(function (text) {
        if (text.trim()) {
          box.appendChild(renderNarrated(turn, round, text));
        }
      });
      var mine = calls.filter(function (call) {
        var of = typeof call.round === "number" ? call.round : 0;
        return of === round;
      });
      if (mine.length > 0) {
        box.appendChild(renderCalls(turn, mine, live));
      }
      (turn.notes || []).forEach(function (note) {
        var of = typeof note.round === "number" ? note.round : 0;
        if (of === round && String(note.text || "").trim()) {
          box.appendChild(renderNote(note.text));
        }
      });
    });
    return box;
  }

  // A line the agent wrote before it went and did something. It is drawn with the answer's own
  // renderer and dimmed, rather than through a second renderer of its own: the words are the same
  // Markdown, and a second one would be a second thing to keep in step. `what` names the round it
  // belongs to, because a code block's copy control has to be told apart from the answer's.
  function renderNarrated(turn, round, text) {
    var box = document.createElement("div");
    box.className = "deepnest-said deepnest-narrated";
    box.appendChild(
      renderWords(text, "round " + round + " of " + copyNames(turn, 0).where)
    );
    return box;
  }

  // What the agent ran, one row each: the tool, the argument that says what it was about, and how
  // it ended. The count this replaced — "ran 12 tools" — was the one thing a person could not
  // check, and the file had every call in it.
  //
  // The output sits behind a disclosure: a turn that read four files is four lines to scan and a
  // great deal not to scroll past, and the whole of it is one press away.
  //
  // Grouping is by the response that asked. A model that asks six things in one answer made one
  // decision, not six, and six equal rows said otherwise; a turn of twenty calls was twenty rows
  // to read past before reaching the answer. A batch of one keeps its bare row — a fold around a
  // single line is a press that hides nothing.
  function renderCalls(turn, calls, live) {
    var box = document.createElement("div");
    box.className = "deepnest-calls";
    groupsOf(calls || []).forEach(function (group) {
      if (group.calls.length === 1) {
        box.appendChild(renderCall(turn, group.calls[0], group.index, live));
        return;
      }
      box.appendChild(renderGroup(turn, group, live));
    });
    return box;
  }

  // The calls of one turn, split where the response changed. `index` is the group's first call's
  // position in the turn, so a disclosure key still names one place across a poll.
  function groupsOf(calls) {
    var groups = [];
    calls.forEach(function (call, index) {
      // A record written before this field existed has no number; every one of those lands in the
      // same group rather than each becoming its own, which is the shape that file had anyway.
      var round = typeof call.round === "number" ? call.round : 0;
      var last = groups.length > 0 ? groups[groups.length - 1] : null;
      if (last !== null && last.round === round) {
        last.calls.push(call);
      } else {
        groups.push({ round: round, calls: [call], index: index });
      }
    });
    return groups;
  }

  // What one batch was, in one line: how many calls, and how they ended. Endings counted rather
  // than summed up as a verdict, because a batch that read four files and failed one is both, and
  // the words are the same ones the rows use.
  function groupWord(calls, live) {
    var counted = {};
    var order = [];
    calls.forEach(function (call) {
      var word = stateWord(call.state, live);
      if (!counted[word]) {
        counted[word] = 0;
        order.push(word);
      }
      counted[word] += 1;
    });
    var head =
      calls.length + (calls.length === 1 ? " tool call" : " tool calls");
    if (order.length === 0) {
      return head;
    }
    return (
      head +
      " \u00b7 " +
      order
        .map(function (word) {
          return counted[word] + " " + word;
        })
        .join(", ")
    );
  }

  // The batch, shut. It opens onto the same rows a lone call gets, so nothing is reachable only
  // one way — and it stays open across a poll for the same reason a call's output does: while a
  // turn runs the transcript is rebuilt every round.
  function renderGroup(turn, group, live) {
    var more = document.createElement("details");
    more.className = "deepnest-calls-group";
    var summary = document.createElement("summary");
    summary.textContent = groupWord(group.calls, live);
    more.appendChild(summary);
    var rows = document.createElement("div");
    rows.className = "deepnest-calls-group-rows";
    group.calls.forEach(function (call, offset) {
      rows.appendChild(renderCall(turn, call, group.index + offset, live));
    });
    more.appendChild(rows);
    var key = turn.at + "\u0000group\u0000" + group.round;
    more.open = !!openedGroups[key];
    more.addEventListener("toggle", function () {
      if (more.open) {
        openedGroups[key] = true;
      } else {
        delete openedGroups[key];
      }
    });
    return more;
  }

  // One call, read the way a person reads a call: what it was for, then how it ended. The
  // arguments are behind a second disclosure with the answer they got.
  //
  // The row used to lead with `what` — whichever argument happened to look most like a subject —
  // and that is a command line or a path: it says what was done and never why. Every tool's schema
  // now makes `intent` a required argument, so the row leads with the model's own sentence about
  // what it was trying to reach, and `what` is only the fallback for a call made without one.
  function renderCall(turn, call, index, live) {
    var row = document.createElement("div");
    row.className = "deepnest-call deepnest-call-" + (call.state || "waiting");

    var name = document.createElement("span");
    name.className = "deepnest-call-name";
    name.textContent = call.name;
    row.appendChild(name);

    // The reason, and the argument under it when there is one to show. `deepnest-call-what` stays
    // on whichever of the two is drawn, because it is the span that takes the row's spare width
    // and ellipsises what does not fit.
    var line = String(call.intent || call.what || "");
    if (line) {
      var what = document.createElement("span");
      what.className = "deepnest-call-what";
      what.textContent = line;
      // The other one, on hover: a sentence about the intent and a command line are two different
      // facts, and the row has room for one of them.
      if (call.intent && call.what && call.what !== call.intent) {
        what.title = call.what;
      }
      row.appendChild(what);
    }

    var state = document.createElement("span");
    state.className = "deepnest-call-state";
    state.textContent = stateWord(call.state, live);
    row.appendChild(state);

    if (call.input || call.output) {
      // Keyed by the turn's own timestamp rather than by position: the list is rebuilt on every
      // poll and an index would move under the row that was open.
      var key = turn.at + "\u0000" + index + "\u0000" + call.name;
      var more = document.createElement("details");
      more.className = "deepnest-call-out";
      var summary = document.createElement("summary");
      summary.textContent = "input and output";
      more.appendChild(summary);
      if (call.input) {
        more.appendChild(callPart("input", call.input));
      }
      if (call.output) {
        more.appendChild(callPart("output", call.output));
      }
      more.open = !!openedCalls[key];
      more.addEventListener("toggle", function () {
        if (more.open) {
          openedCalls[key] = true;
        } else {
          delete openedCalls[key];
        }
      });
      row.appendChild(more);
    }
    return row;
  }

  // One half of the disclosure: which half it is, then the text. The label is a node rather than a
  // `pre` prefix because a tool's answer is arbitrary text and a prefix would sit inside the thing
  // it labels — copy it, and the word comes with it.
  function callPart(label, text) {
    var box = document.createElement("div");
    box.className = "deepnest-call-part";
    var head = document.createElement("p");
    head.className = "deepnest-call-part-name";
    head.textContent = label;
    var pre = document.createElement("pre");
    pre.textContent = text;
    box.appendChild(head);
    box.appendChild(pre);
    return box;
  }

  // ---- The answer, as the model wrote it ----------------------------------------------------

  // The model writes Markdown — lists, `code`, **emphasis**, fenced blocks — and the transcript
  // showed that source to the person. This renders the small subset that actually appears, and it
  // builds nodes instead of markup: every character here was written by a model, and `textContent`
  // is the only way those words reach a page without also deciding what the page does.
  var INLINE = /(`[^`\n]+`|\*\*[^*\n]+\*\*|\*[^*\n]+\*|\[[^\]\n]+\]\(https?:\/\/[^)\s]+\))/;

  function withInline(into, text) {
    var rest = text;
    while (rest.length > 0) {
      var found = INLINE.exec(rest);
      if (!found) {
        into.appendChild(document.createTextNode(rest));
        return;
      }
      if (found.index > 0) {
        into.appendChild(document.createTextNode(rest.slice(0, found.index)));
      }
      var piece = found[0];
      if (piece.charAt(0) === "`") {
        var code = document.createElement("code");
        code.textContent = piece.slice(1, -1);
        into.appendChild(code);
      } else if (piece.slice(0, 2) === "**") {
        var strong = document.createElement("strong");
        withInline(strong, piece.slice(2, -2));
        into.appendChild(strong);
      } else if (piece.charAt(0) === "*") {
        var em = document.createElement("em");
        withInline(em, piece.slice(1, -1));
        into.appendChild(em);
      } else {
        var close = piece.indexOf("]");
        var link = document.createElement("a");
        link.textContent = piece.slice(1, close);
        link.href = piece.slice(close + 2, -1);
        // Only `http:` and `https:` — the pattern refuses anything else, so a `javascript:` link a
        // model was talked into writing is text, not a link.
        link.target = "_blank";
        link.rel = "noopener noreferrer";
        into.appendChild(link);
      }
      rest = rest.slice(found.index + piece.length);
    }
  }

  // A list line, in the three parts its shape is made of: how far in it starts, which marker it
  // uses, and the words. The indent is kept rather than trimmed away, because it is the only thing
  // that says an item sits *under* the item above it rather than after it.
  var BULLET = /^([ \t]*)([-*+]|\d+[.)])\s+(.*)$/;
  var HEADING = /^(#{1,4})\s+(.*)$/;

  // ---- Tables ---------------------------------------------------------------------------------

  // An agent answers a comparison with a table more often than with anything else, and none of the
  // shapes above caught one: every row of it fell through to the paragraph branch, which joins its
  // lines with a space. Measured in Chrome against the running binary, a reply holding a
  // three-column table of four rows drew **one paragraph**, 320 px wide and 88 px tall, whose text
  // ran `| Model | Price per 1M input | Verdict | |---|--:|:--:| | kimi-k2.6 | 4.00 | …` — twenty
  // pipe characters in a single run of prose, the delimiter row included. Nothing on the page was a
  // `table`, so nothing lined up under anything: the verdict for one model sat beside the price of
  // the next.
  //
  // The delimiter row is the whole signal. A line is a header when the line under it is made only
  // of `-`, `:` and `|`, which is exactly what separates a table from prose that happens to carry a
  // pipe, and the colons in it are what the columns are aligned by.
  function tableRule(line) {
    var body = String(line).trim();
    if (body.indexOf("-") < 0) {
      return null;
    }
    if (body.charAt(0) === "|") {
      body = body.slice(1);
    }
    if (body.charAt(body.length - 1) === "|") {
      body = body.slice(0, -1);
    }
    var parts = body.split("|");
    var align = [];
    for (var n = 0; n < parts.length; n++) {
      var one = parts[n].trim();
      if (!/^:?-+:?$/.test(one)) {
        return null;
      }
      var left = one.charAt(0) === ":";
      var right = one.charAt(one.length - 1) === ":";
      align.push(left && right ? "center" : right ? "right" : left ? "left" : "");
    }
    return align;
  }

  // One row's cells. The outer pipes are optional — a model writes a table with them and without —
  // and `\|` is a cell that is meant to contain a bar rather than end at one.
  function tableCells(line) {
    var body = String(line).trim();
    if (body.charAt(0) === "|") {
      body = body.slice(1);
    }
    if (body.charAt(body.length - 1) === "|") {
      body = body.slice(0, -1);
    }
    var cells = [];
    var at = "";
    for (var n = 0; n < body.length; n++) {
      var ch = body.charAt(n);
      if (ch === "\\" && body.charAt(n + 1) === "|") {
        at += "|";
        n += 1;
        continue;
      }
      if (ch === "|") {
        cells.push(at);
        at = "";
        continue;
      }
      at += ch;
    }
    cells.push(at);
    return cells.map(function (one) {
      return one.trim();
    });
  }

  // Does a table start at this line? Asked from the paragraph branch as well, because a model
  // writes "the numbers are:" and the table on the next line with no blank line between them, and
  // a paragraph that kept reading would swallow the header row before this loop ever saw it.
  function tableHere(lines, i) {
    if (i < 0 || i + 1 >= lines.length) {
      return null;
    }
    if (String(lines[i]).indexOf("|") < 0) {
      return null;
    }
    return tableRule(lines[i + 1]);
  }

  function tableCell(tag, text, align) {
    var cell = document.createElement(tag);
    if (align) {
      cell.className = "deepnest-table-" + align;
    }
    withInline(cell, text);
    return cell;
  }

  // What a control is *called*, which is all a person with a screen reader has to go on.
  //
  // The transcript drew twelve controls with two names between them: measured in Chrome's own
  // accessibility tree against six exchanges, the announced names were "Copy this code block" and
  // "Copy this answer", each six times over, and nothing anywhere said which exchange a control
  // belonged to. A person tabbing through could copy the wrong answer twice before noticing.
  //
  // So every control names the exchange it sits in, and the exchange is named by what was asked —
  // the one thing about a turn a person recognizes without reading the turn.
  var NAME_THE_QUESTION = 60;

  // Both names an exchange's controls use, built once: `where` is the place a block sits in, and
  // `answer` is the whole-answer control. They read differently — "the answer to X" and "a block in
  // the answer to X" — because each has to make sense on its own, and a message that carried only a
  // picture has no question to quote, so both fall back on the position.
  function copyNames(turn, number) {
    var question = String(turn.user || "")
      .replace(/\s+/g, " ")
      .trim();
    if (question.length > NAME_THE_QUESTION) {
      question = question.slice(0, NAME_THE_QUESTION - 1).trim() + "…";
    }
    if (!question) {
      var place = number > 0 ? "exchange " + number : "this answer";
      return { where: place, answer: "Copy the answer in " + place };
    }
    return {
      where: 'the answer to "' + question + '"',
      answer: 'Copy the answer to "' + question + '"'
    };
  }

  // A block's own name: which block, in which answer. The ordinal is only worth saying when there
  // is more than one, because "1 of 1" is a number that tells nobody anything.
  function blockLabel(language, what, ordinal, total) {
    var kind = language ? language + " code block" : "code block";
    if (total > 1) {
      return "Copy the " + kind + " " + ordinal + " of " + total + " in " + what;
    }
    return "Copy the " + kind + " in " + what;
  }

  function renderWords(text, what) {
    var box = document.createDocumentFragment();
    var lines = String(text).split("\n");
    // How many blocks this answer holds, counted before any is drawn, because a block's name says
    // which one it is and that needs the total as well as the position. A fence with no closing
    // fence is still a block — the loop below draws it as one — so the odd fence rounds up.
    var fences = 0;
    lines.forEach(function (one) {
      if (one.indexOf("```") === 0) {
        fences += 1;
      }
    });
    var blocks = Math.ceil(fences / 2);
    var drawn = 0;
    var i = 0;
    while (i < lines.length) {
      var line = lines[i];
      if (!line.trim()) {
        i++;
        continue;
      }
      if (line.indexOf("```") === 0) {
        // The fence's own line carries the one word a reader uses: `rust` above a block of Rust is
        // how they know it is not a shell command. It used to be dropped on the floor here.
        var language = line.slice(3).trim();
        var body = [];
        i++;
        while (i < lines.length && lines[i].indexOf("```") !== 0) {
          body.push(lines[i]);
          i++;
        }
        i++;
        drawn += 1;
        box.appendChild(fencedBlock(language, body.join("\n"), what, drawn, blocks));
        continue;
      }
      var align = tableHere(lines, i);
      if (align) {
        var table = document.createElement("table");
        var head = document.createElement("thead");
        var headRow = document.createElement("tr");
        tableCells(line).forEach(function (text, column) {
          headRow.appendChild(tableCell("th", text, align[column]));
        });
        head.appendChild(headRow);
        table.appendChild(head);
        // The delimiter row is scaffolding, not a row: it says how wide the table is and where each
        // column leans, and drawing it would put the punctuation of the format into the answer.
        i += 2;
        var rows = document.createElement("tbody");
        while (i < lines.length && lines[i].trim() && lines[i].indexOf("|") >= 0) {
          var row = document.createElement("tr");
          tableCells(lines[i]).forEach(function (text, column) {
            row.appendChild(tableCell("td", text, align[column]));
          });
          rows.appendChild(row);
          i += 1;
        }
        if (rows.childNodes.length > 0) {
          table.appendChild(rows);
        }
        box.appendChild(table);
        continue;
      }
      var bullet = BULLET.exec(line);
      var heading = HEADING.exec(line);
      if (heading) {
        // Three levels down from an h1: the box is not a document, and a heading the size of the
        // panel title would outrank the app's own name.
        var level = Math.min(6, heading[1].length + 2);
        var title = document.createElement("h" + level);
        withInline(title, heading[2]);
        box.appendChild(title);
        i++;
        continue;
      }
      if (bullet) {
        // The lists this run has opened, innermost last. The version this replaced took every
        // consecutive bullet line into one list, whatever its indent and whatever its marker, so
        // an answer whose steps carried their own sub-points drew the sub-points as steps: "1. read
        // the file, 2. the header row, 3. the first data row, 4. fix the import" — and the
        // numbering was this page's invention rather than the model's. Measured in Chrome against
        // the running binary: six `li` in one `ol`, every one of them `decimal`, and not one list
        // inside a list anywhere in the answer.
        var open = [];
        while (i < lines.length) {
          var item = BULLET.exec(lines[i]);
          if (!item) {
            break;
          }
          // A tab is one step in, counted as the two spaces it is: a model writes one or the other
          // throughout, and two lists whose indents agree are lists of the same depth.
          var at = item[1].replace(/\t/g, "  ").length;
          var ordered = /\d/.test(item[2]);
          // Out: a line less far in than the innermost list is not an item of it, so that list —
          // and any list inside it that this line is also out of — closes.
          while (open.length > 0 && open[open.length - 1].at > at) {
            open.pop();
          }
          // The other marker at the same depth is a second list rather than a second item: a `-`
          // under a `1.` is a bulleted list of its own, and drawing it as a numbered step makes
          // the answer say something it did not say.
          if (open.length > 0) {
            var innermost = open[open.length - 1];
            if (innermost.at === at && innermost.ordered !== ordered) {
              open.pop();
            }
          }
          // In: nothing open, or a list that starts further out than this line does, means this
          // line opens a list — inside the item above it when there is one, and inside the answer
          // itself when there is not.
          var parent = open[open.length - 1];
          if (!parent || parent.at < at) {
            var list = document.createElement(ordered ? "ol" : "ul");
            if (parent) {
              parent.list.lastChild.appendChild(list);
            } else {
              box.appendChild(list);
            }
            parent = { at: at, ordered: ordered, list: list };
            open.push(parent);
          }
          var point = document.createElement("li");
          withInline(point, item[3]);
          parent.list.appendChild(point);
          i++;
        }
        continue;
      }
      var words = [];
      while (
        i < lines.length &&
        lines[i].trim() &&
        lines[i].indexOf("```") !== 0 &&
        !BULLET.test(lines[i]) &&
        !HEADING.test(lines[i]) &&
        !tableHere(lines, i)
      ) {
        words.push(lines[i]);
        i++;
      }
      var paragraph = document.createElement("p");
      withInline(paragraph, words.join(" "));
      box.appendChild(paragraph);
    }
    return box;
  }

  // ---- Taking the answer out of the page -----------------------------------------------------

  // An answer here is usually code, and code is written to be used somewhere else. Two scopes: a
  // fenced block carries its own control, because that is the unit a person actually needs, and an
  // answer with no fence at all still gets one for the whole of it.
  var COPY = "Copy";
  var COPIED = "Copied";
  var COPY_FAILED = "Copy failed";

  function saidSo(control, took) {
    control.textContent = took ? COPIED : COPY_FAILED;
    control.setAttribute("data-copied", took ? "yes" : "no");
    if (control.deepnestWord) {
      clearTimeout(control.deepnestWord);
    }
    control.deepnestWord = setTimeout(function () {
      control.textContent = COPY;
      control.removeAttribute("data-copied");
      control.deepnestWord = null;
    }, 1400);
  }

  // The route that needs no permission. A `textarea` the browser itself selects makes the copy the
  // platform's own rather than this page's idea of one, and it is removed again in the same turn of
  // the event loop, so no node of it is left in the app's page.
  function copyTheOldWay(text) {
    var holder = document.createElement("textarea");
    holder.value = text;
    holder.setAttribute("readonly", "");
    holder.setAttribute("aria-hidden", "true");
    holder.style.position = "fixed";
    holder.style.top = "-1000px";
    holder.style.opacity = "0";
    document.body.appendChild(holder);
    holder.select();
    var took = false;
    try {
      took = document.execCommand("copy");
    } catch (error) {
      // A browser that no longer implements it throws rather than returning false.
      took = false;
    }
    document.body.removeChild(holder);
    return took;
  }

  // The third case is not a failure message. Handing the job back means the text is selected, so
  // the key the person already knows finishes it.
  function selectTheText(node) {
    try {
      var range = document.createRange();
      range.selectNodeContents(node);
      var pick = window.getSelection();
      pick.removeAllRanges();
      pick.addRange(range);
    } catch (error) {
      // A page that cannot select has still left the text on screen, which is where it started.
    }
  }

  function copyText(text, control, node) {
    function refused() {
      saidSo(control, false);
      if (node) {
        selectTheText(node);
      }
    }
    if (navigator.clipboard && navigator.clipboard.writeText) {
      // `writeText` rejects on a page the browser does not consider secure, and a deepnest reached
      // at a LAN address is exactly that — so the fallback is reached by rejection, not by absence,
      // and both have to be wired for the button to work on every page this dialog lands on.
      navigator.clipboard.writeText(text).then(
        function () {
          saidSo(control, true);
        },
        function () {
          if (copyTheOldWay(text)) {
            saidSo(control, true);
          } else {
            refused();
          }
        }
      );
      return;
    }
    if (copyTheOldWay(text)) {
      saidSo(control, true);
    } else {
      refused();
    }
  }

  function copyControl(className, what, text, node) {
    var copy = document.createElement("button");
    copy.type = "button";
    copy.className = className;
    copy.textContent = COPY;
    copy.setAttribute("aria-label", what);
    copy.addEventListener("click", function () {
      copyText(text, copy, node);
    });
    return copy;
  }

  // How much of a long block is drawn before the rest is folded away, and the block has to be at
  // least twice this to be worth folding at all.
  //
  // Measured in Chrome against two answers that each wrote a 60-line module: the panel is 512 px
  // tall, the transcript's own scroll box is 280 px, a row of code is 16 px (11 px monospace on
  // 15.95 px leading) so that window holds seventeen rows, and one block was 969 px — 3.5 windows
  // — which made one exchange 1076 px. With the transcript at its newest end, which is where it
  // lands after every turn, the exchange before that one sat at -1874 px: an answer that wrote a
  // file had pushed the whole conversation out of reach, and the code itself was still mostly
  // below the fold.
  //
  // Fourteen rows is 224 px, four fifths of the window, so a folded block and the sentence above
  // it are one exchange tall. Folding only starts at 28 rows (448 px, one and a half windows):
  // under that, a control that reveals fifteen lines costs more attention than the scrolling it
  // saves. Measured after: the exchange went from 1076 px to 365 px, and both of them from 2154 px
  // to 731 px.
  var BLOCK_LINES_SHOWN = 14;

  // Blocks a person has opened, keyed by the answer they sit in and their position in it. The
  // transcript is rebuilt whenever the record grew, which while a turn runs is every round, and a
  // block that folded itself back up under the reader would be a block nobody could read past.
  var openedBlocks = {};

  // A fenced block. The head sits above the text and holds the language the fence named and the
  // copy, so pressing one of them cannot move the code the person is reading — and `pre` is what
  // scrolls sideways, so a control inside it would scroll out of reach.
  function fencedBlock(language, source, what, ordinal, total) {
    var block = document.createElement("div");
    block.className = "deepnest-code";

    var pre = document.createElement("pre");
    var code = document.createElement("code");
    // One element per line of the source, rather than all of it as one text node. The column is
    // 19rem wide and a real line of Rust does not fit in it, so the block wraps — and every row used
    // to start at the block's own left edge, which made one statement read as several. Measured in
    // Chrome against a four-line block whose second line was 122 characters: the browser drew six
    // rows, all of them flush at the same x, so `let totals = rows.iter()…sum::<i64>();` read as
    // three lines with the last of them starting `…sum::<i64>();`.
    //
    // A span per line is what lets the stylesheet pull the first row of a line back to the margin and
    // push the wrapped rows in, which is the difference between a continuation and a new line.
    function draw(rows) {
      rows.forEach(function (one) {
        var line = document.createElement("span");
        line.className = "deepnest-code-line";
        // An empty line is still a line: an empty block generates no line box at all, so the blank
        // row would vanish and the code below it would move up into its place.
        line.textContent = one === "" ? "\u200b" : one;
        code.appendChild(line);
      });
    }

    var rows = String(source).split("\n");
    // Only a recorded block folds. The sentence being written goes through this same renderer with
    // `what` null, and what it must do is show its newest line; hiding the end of it behind a
    // control would take away the one thing a reader watching a turn needs.
    var key = what === null ? null : what + "\u0000" + ordinal;
    var folded = false;
    if (key !== null && !openedBlocks[key] && rows.length > BLOCK_LINES_SHOWN * 2) {
      draw(rows.slice(0, BLOCK_LINES_SHOWN));
      folded = true;
    } else {
      draw(rows);
    }
    pre.appendChild(code);

    // `what` is null where the text is being *shown* rather than copied from — the answer line under
    // the box, which is a view of the turn the transcript above already holds. A block there gets no
    // head and no control: the language tag would be the only thing in the row, and a copy button
    // would be a second set of tab stops for a control the transcript already offers.
    if (what === null) {
      block.appendChild(pre);
      return block;
    }

    var head = document.createElement("div");
    head.className = "deepnest-code-head";

    var label = document.createElement("span");
    label.className = "deepnest-code-language";
    // An unnamed fence is still a code block, and saying so beats an empty corner.
    label.textContent = language || "code";

    head.appendChild(label);
    head.appendChild(
      copyControl(
        "deepnest-code-copy",
        blockLabel(language, what, ordinal, total),
        source,
        code
      )
    );
    block.appendChild(head);
    block.appendChild(pre);

    if (folded) {
      var hidden = rows.length - BLOCK_LINES_SHOWN;
      var more = document.createElement("button");
      more.type = "button";
      more.className = "deepnest-code-more";
      // The code's own words, not just a count: "Show 17 more lines" beside two blocks leaves a
      // person guessing which one it belongs to, and a screen reader with no block at all.
      more.textContent =
        "Show the other " +
        hidden +
        (hidden === 1 ? " line" : " lines") +
        " of this block";
      more.setAttribute(
        "aria-label",
        "Show the other " +
          hidden +
          (hidden === 1 ? " line" : " lines") +
          " of the " +
          (language ? language + " code block" : "code block") +
          (total > 1 ? " " + ordinal + " of " + total : "") +
          " in " +
          what
      );
      more.addEventListener("click", function () {
        // Opened stays opened: the transcript is redrawn from the record on the next poll, and the
        // key is what carries the press across that redraw. The redraw itself is done here rather
        // than fetched, because the lines are already in this page and the record has not changed.
        openedBlocks[key] = true;
        draw(rows.slice(BLOCK_LINES_SHOWN));
        more.remove();
      });
      block.appendChild(more);
    }
    return block;
  }

  // `number` is where this turn sits in the conversation, counting from the first exchange it has:
  // the fallback name for a message that carried no words, and nothing else.
  function renderTurn(turn, live, number) {
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
    //
    // The words get an element of their own because the time shares this row: a transcript of a
    // conversation held over days said nothing about when any of it happened, so two exchanges
    // that read the same were told apart by nothing. The row rather than a line of its own, because
    // height in this panel is what #85 and #86 were spent on.
    var words = document.createElement("span");
    words.className = "deepnest-asked-words";
    words.textContent =
      turn.user || (pictures + files > 0 ? withWhat(pictures, files) : "");
    asked.appendChild(words);

    var instant = instantOf(turn.at);
    if (instant !== null) {
      var when = document.createElement("time");
      when.className = "deepnest-turn-time";
      // The record's own instant, unchanged: the text is one reading of it, and the file is the
      // thing another reader — or a machine — has to be able to agree with.
      when.setAttribute("datetime", turn.at);
      when.textContent = sameLocalDay(instant, new Date())
        ? clockOf(instant)
        : dayOf(instant) + " " + clockOf(instant);
      when.setAttribute("title", whenOf(turn.at));
      asked.appendChild(when);
    }

    var calls = turn.calls || [];
    var did = document.createElement("p");
    did.className = "deepnest-did";
    // Only when there are no rows to draw — a count is what a transcript falls back on, and the
    // rows say the same thing better.
    if (turn.tools > 0 && calls.length === 0) {
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
    // The calls and the words between them, in the order they happened. The answer is drawn after
    // it, because the answer is what the turn ended with.
    var stream = renderStream(turn, live);
    if (stream) {
      line.appendChild(stream);
    }
    // The text in flight is the round this turn is on, and it goes exactly where the record will
    // put those same words when the round ends: at the end of the running turn. It is drawn with
    // `fillWriting` rather than `renderNarrated` because a sentence still growing must not offer a
    // copy control that would copy half of it.
    if (live && writing) {
      writingNode = document.createElement("div");
      writingNode.className = "deepnest-narrated deepnest-writing";
      line.appendChild(writingNode);
      fillWriting();
    } else if (live) {
      writingNode = null;
    }
    if (turn.reply) {
      var said = document.createElement("div");
      said.className = "deepnest-said";
      var names = copyNames(turn, number);
      said.appendChild(renderWords(turn.reply, names.where));
      // The whole answer, as the model wrote it. What a person pastes elsewhere wants the same
      // Markdown they are reading, and rebuilding it out of the drawn nodes would be a second
      // renderer to keep in step with the first.
      said.appendChild(
        copyControl("deepnest-answer-copy", names.answer, turn.reply)
      );
      line.appendChild(said);
    }
    return line;
  }

  transcript.addEventListener("scroll", function () {
    offerTheLatest(atTheLatest());
  });

  // What was last drawn, so a poll that found nothing new does not redraw: a transcript rebuilt
  // every second would fight the person scrolling through it.
  var drawn = "";

  // The turns the window has not reached yet, one press away. It is a control rather than a note
  // because reading the start of a conversation is something a person does, and the count is what
  // makes the gap visible: a transcript that quietly dropped its first exchanges would look like a
  // conversation that began where the drawing began.
  function drawEarlier(older) {
    var more = document.createElement("button");
    more.type = "button";
    more.id = "deepnest-dialog-earlier";
    more.textContent =
      older === 1
        ? "Show the 1 earlier message"
        : "Show the " + older + " earlier messages";
    more.addEventListener("click", function () {
      shownTurns += TURNS_SHOWN;
      // Turns are about to appear above the ones on screen, so the next redraw holds the reader's
      // place instead of leaving them at the top of a list that grew over their head.
      holdingPlace = true;
      readHistory();
    });
    return more;
  }

  // What an exchange is searched on: the words the person said, the words the agent answered, the
  // notes, and what each tool call was about. Those are the four things the transcript draws, so a
  // phrase a person can see is a phrase this finds — a search narrower than the page would report
  // "no exchange matches" about words on the screen.
  function matches(turn, needle) {
    var fields = [turn.user || "", turn.reply || ""].concat(
      (turn.notes || []).map(function (note) {
        return String(note.text || "");
      })
    );
    (turn.calls || []).forEach(function (call) {
      fields.push(call.name || "");
      fields.push(call.what || "");
    });
    var haystack = fields.join("\n").toLowerCase();
    return haystack.indexOf(needle) >= 0;
  }

  function showTurns(turns, live) {
    if (!turns || turns.length === 0) {
      drawn = "";
      shownTurns = TURNS_SHOWN;
      holdingPlace = false;
      finding = "";
      find.hidden = true;
      found.hidden = true;
      transcript.textContent = "";
      // A conversation with no exchange yet still has somewhere to say that the first message could
      // not be sent, which is the one moment a person is looking at an empty transcript.
      drawReports();
      transcript.hidden = notices.length === 0 && !spendLine;
      return false;
    }
    // Numbered first: a search draws exchanges from anywhere in the conversation, and each one has
    // to keep the number it has in the conversation rather than the position it landed in on
    // screen. The drawer counts the same way, so "exchange 7" means one thing everywhere.
    var numbered = turns.map(function (turn, index) {
      return { turn: turn, number: index + 1 };
    });
    var recent;
    var older;
    if (finding) {
      // A search is not a page: it looks through the whole conversation, and it says so, because
      // "no exchange matches" is a different answer from "not in the twenty on screen".
      recent = numbered.filter(function (entry) {
        return matches(entry.turn, finding);
      });
      older = 0;
    } else {
      recent = numbered.slice(-shownTurns);
      older = numbered.length - recent.length;
    }
    // Whether the transcript is about to hold the newest answer there is: the last exchange is among
    // the ones being drawn, and the record already has its reply. The answer box handing its copy
    // over (below, in the submit path) turns on exactly this — a search that has filtered the newest
    // turn out, or a record whose reply has not been written yet, answers false and the box stays
    // the only copy, rather than the page leaving a person with neither.
    var newest = numbered[numbered.length - 1];
    var drawsTheAnswer = recent.indexOf(newest) !== -1 && !!newest.turn.reply;
    find.hidden = older === 0 && !finding;
    found.hidden = !finding;
    if (finding) {
      found.textContent =
        recent.length === 0
          ? "No exchange matches that."
          : "Showing " +
            recent.length +
            " of " +
            numbered.length +
            (numbered.length === 1 ? " exchange" : " exchanges");
    }
    // What the memo compares: the conversation as the file has it, whether a turn is running, the
    // phrase being searched for, and the words in flight. The last of those is the one that was
    // missing — a redraw asked for because the answer had grown compared equal to the one before it
    // and was skipped, so a stream could run to its end with nothing on screen but the question.
    var print =
      JSON.stringify(recent) + (live ? "+" : "") + "|" + finding + "|" + writing;
    if (print === drawn) {
      // Nothing was redrawn, so there is no place to hold: a flag kept for a redraw that did not
      // happen would be spent on the next one, which moved something else.
      holdingPlace = false;
      return drawsTheAnswer;
    }
    drawn = print;
    // Read the scroll position before replacing the children, and restore it only if the person was
    // already at the newest end. Auto-scrolling a reader who scrolled up is the page deciding it
    // knows better, which is the one thing a transcript being written to must not do.
    var atTheEnd = atTheLatest();
    var wasTop = transcript.scrollTop;
    var wasTall = transcript.scrollHeight;
    var holding = holdingPlace;
    holdingPlace = false;
    transcript.textContent = "";
    if (older > 0) {
      transcript.appendChild(drawEarlier(older));
    }
    recent.forEach(function (entry, index) {
      transcript.appendChild(
        // `entry.number` is where this turn stands in the conversation, not in the window: the
        // drawer says "exchange 12" of the same counting, so a fallback name and the drawer agree
        // instead of numbering the same turn two ways — and a searched-for exchange keeps its own
        // number however few of its neighbours are on screen.
        renderTurn(entry.turn, live && entry.number === numbered.length, entry.number)
      );
    });
    transcript.hidden = false;
    if (atTheEnd) {
      // Newest at the bottom, which is where a conversation is read from.
      transcript.scrollTop = transcript.scrollHeight;
    } else if (holding) {
      // The same line as before, seen from a box that grew above it: what was added is exactly the
      // distance it moved, so the reader stays on the words they were reading instead of being
      // carried up to the top of a list they only asked to be longer.
      transcript.scrollTop = wasTop + (transcript.scrollHeight - wasTall);
    }
    drawReports();
    offerTheLatest(atTheEnd);
    return drawsTheAnswer;
  }

  // What this page has to report, at the end of the conversation: the rows it wrote itself, and
  // what the last turn cost. Both are about the turn rather than said in it, which is why they come
  // after every exchange instead of inside one.
  function drawReports() {
    notices.forEach(function (notice) {
      var row = document.createElement("p");
      row.className = notice.bad ? "deepnest-report deepnest-report-bad" : "deepnest-report";
      row.textContent = notice.text;
      transcript.appendChild(row);
    });
    if (spendLine) {
      var cost = document.createElement("p");
      cost.className = "deepnest-report deepnest-report-cost";
      cost.textContent = spendLine;
      transcript.appendChild(cost);
    }
  }

  // Whether the reader is at the newest end of the transcript.
  function atTheLatest() {
    return (
      transcript.scrollTop + transcript.clientHeight >= transcript.scrollHeight - 8
    );
  }

  // Offer the way back rather than taking it: hidden while the reader is at the end, hidden when
  // there is nothing to scroll, and shown only to someone who scrolled away from the newest line.
  function offerTheLatest(atTheEnd) {
    var overflows = transcript.scrollHeight > transcript.clientHeight + 8;
    jump.hidden = atTheEnd || !overflows || transcript.hidden;
  }

  function readHistory() {
    return fetch(historyRoute(), { credentials: "same-origin" })
      .then(function (response) {
        return response.ok ? response.json() : null;
      })
      .then(function (body) {
        var turns = (body && body.turns) || [];
        // Before anything is drawn, because the box's own walk is part of what this read is for:
        // the conversation on the server is reachable from the box only if the list is in step
        // with the file, and it is this read that knows what the file holds.
        seedFromTheFile(turns);
        read = turns;
        isLive = watching !== null;
        // A round is recorded the moment it ends, so a round count that has moved since the last
        // look means the words being held here are in the file and are about to be drawn from it.
        // Holding them as well is the duplication this whole change is about.
        var lastTurn = turns.length ? turns[turns.length - 1] : null;
        var rounds = lastTurn ? Number(lastTurn.rounds) || 0 : 0;
        if (isLive) {
          if (writingBase === null) {
            writingBase = rounds;
          } else if (rounds !== writingBase) {
            writingBase = rounds;
            writing = "";
            writingNode = null;
          }
        }
        var drawnTheAnswer = showTurns(turns, isLive);
        if (isLive) {
          showProgress(describeProgress(turns));
        }
        // Handed back rather than read off the panel afterwards: only the caller that has just been
        // given an answer knows which answer it is looking for the transcript to hold.
        return drawnTheAnswer;
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
    // The turn is over, however it ended, so a stop is no longer in flight. Clearing this here
    // rather than at each exit is what keeps the two in step: every exit from a turn goes through
    // this function, and a flag left standing would silence the next turn's progress line.
    stopping = false;
    stop.textContent = STOP_LABEL;
    // And the line goes with it: a reading of a turn that ended is a claim about a turn that is
    // not running. What the turn did is in the transcript, which is the record and outlives it.
    clearStatus();
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

  // What the last turn cost, under the answer it paid for.
  //
  // The numbers are the provider's own, summed over the turn's rounds on the server: a turn is not
  // one request but many, and each one sends the conversation again, so the cost of a turn is not
  // the cost of its answer. Shown rather than calculated here — a page that multiplied a guessed
  // price by the length of the text would be a second meter disagreeing with the one the provider
  // sends, and the disagreement would be invisible.
  // Thousands separators, because the numbers that matter here are five and six digits long and a
  // reader counting zeroes is a reader not reading the number. `toLocaleString` rather than a
  // hand-rolled grouping: it is the one piece of this that the platform already does correctly for
  // every locale, and a turn's cost is not a place to have an opinion about separators.
  function counted(n) {
    return Number(n || 0).toLocaleString();
  }

  function showSpend(body) {
    var usage = body && body.usage;
    // Nothing reported and nothing spent are different readings with the same page: a provider that
    // does not say (some self-hosted endpoints answer no `usage` at all) would otherwise be shown
    // as a turn that was free. Silence is the honest rendering of "not reported".
    if (!usage || !(Number(usage.prompt_tokens) || Number(usage.completion_tokens))) {
      spendLine = "";
      drawNow();
      return;
    }
    var said = "This turn: " + counted(usage.prompt_tokens) + " tokens in";
    if (Number(usage.cached_tokens)) {
      // Worth naming rather than folding in: on a long turn most of the prompt is the same history
      // sent again, and how much of it the provider served from its cache is the one number that
      // says whether re-sending it is still being paid for at full price.
      said += " (" + counted(usage.cached_tokens) + " from cache)";
    }
    said += ", " + counted(usage.completion_tokens) + " out";
    var rounds = Number(body && body.rounds);
    if (rounds) {
      said += " \u2014 " + rounds + (rounds === 1 ? " round" : " rounds");
    }
    spendLine = said + ".";
    drawNow();
  }

  function watchTheTurn() {
    sentAt = Date.now();
    stopWatching();
    // Rows this page wrote about the last turn leave with it, and so does its cost: a number left
    // in the conversation while a new turn runs is a number about something else.
    notices = [];
    spendLine = "";
    // Nothing is held from the turn before: its words are in the file, and the count this page
    // measures the next round against is taken from the file rather than carried over.
    writing = "";
    writingNode = null;
    writingBase = null;
    clearStatus();
    // There is a turn now, and it can take minutes: this is the one moment the page can offer a
    // way out of it.
    stop.hidden = false;
    stop.disabled = false;
    stop.textContent = STOP_LABEL;
    stopping = false;
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

  // What a finished build reloads.
  //
  // The frame, when there is one: the app has a new binary behind it and this page has nothing to
  // learn — the conversation is here, and reloading this page is what used to take it away. The
  // tab, when there is no frame: the older shape, where the app *is* the page.
  function reloadTheApp() {
    if (stage !== null) {
      try {
        stage.contentWindow.location.reload();
        return;
      } catch (error) {
        // Same origin, so this should not happen. If it does, falling through reloads the tab —
        // the app on the old build with the conversation gone is still better than a person left
        // watching a page that will never change.
      }
    }
    location.reload();
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
            reloadTheApp();
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
      // The answer arrives as it is written rather than all at once at the end. This header is the
      // whole of what asks for that, and it costs nothing when the route has nothing to stream:
      // a refusal is answered with a JSON body either way, and `wantsEvents` reads the reply.
      headers: { "Content-Type": "application/json", Accept: STREAM },
      body: JSON.stringify(body)
    });
  }

  // Enter sends the message, Shift+Enter starts a line — the shape every agent front end has, and
  // the one this box did not. Measured in Chrome with a real key press, on the box holding "fix the
  // failing tests": `Enter` reached the textarea as a `keydown`, the browser turned it into a line
  // break, the form was submitted **0** times and the stub saw **0** chat requests. So a person who
  // types a sentence and presses Enter gets a second line and has to reach for the mouse, while
  // Claude Code, Codex and the harness this project is modelled on all send on Enter.
  //
  // `requestSubmit` rather than `.click()` on the button: the button is disabled for the length of
  // a turn, and that would silently turn the key off. The handler below is the one place that knows
  // what sending means, so Enter goes through it instead of repeating any of it here.
  input.addEventListener("keydown", function (event) {
    if (event.key !== "Enter" || event.shiftKey) {
      return;
    }
    // An input method's Enter belongs to the input method. The candidate window is open and the
    // person is choosing a character: sending here would post half a word and lose the rest. The
    // host writes Chinese, so this is not a hypothetical — and `keyCode` is the same guard for the
    // browsers that report composition as 229 instead of setting `isComposing`.
    if (event.isComposing || event.keyCode === 229) {
      return;
    }
    // Nothing can be sent during a turn, so the key is left to do what it visibly does — a line
    // break — rather than becoming a no-op that reads as a broken key.
    if (send.disabled) {
      return;
    }
    event.preventDefault();
    // A fresh message ends the walk: the next `Up` should start again from the newest one sent, not
    // from wherever the last walk stopped.
    recallAt = 0;
    recalled = false;
    form.requestSubmit();
  });

  // `Up` in an empty box walks back through the messages already sent, and `Down` walks forward
  // again, ending on the empty box it started from.
  //
  // Both keys are left alone unless the box holds nothing the person typed: a person moving the
  // caret around a sentence they are writing is doing something else entirely, and taking the key
  // for a history walk would replace the paragraph under them. Measured against the box holding
  // "half a thought": the browser itself kept the text (`value` unchanged), and that is the
  // behaviour this must not break. An empty box is the easy case — there is no caret to move — and
  // a recalled message counts as empty-able because nobody has edited it yet.
  input.addEventListener("keydown", function (event) {
    if (event.key !== "ArrowUp" && event.key !== "ArrowDown") {
      return;
    }
    if (event.isComposing) {
      return;
    }
    if (input.value !== "" && !recalled) {
      return;
    }
    if (event.key === "ArrowUp") {
      var back = recallList();
      if (recallAt >= back.length) {
        // At the oldest message: nothing left to walk to, so the key changes nothing rather than
        // emptying a box the person is reading. The default is still taken away, because otherwise
        // the browser moves the caret to the start of the message the person is looking at.
        event.preventDefault();
        return;
      }
      recallAt += 1;
      event.preventDefault();
      putBack(back[back.length - recallAt]);
      return;
    }
    // `Down`. Past the newest message is the empty box, which is what a person who walked back and
    // then changed their mind expects to arrive at.
    if (recallAt === 0) {
      return;
    }
    recallAt -= 1;
    event.preventDefault();
    var forward = recallList();
    putBack(recallAt === 0 ? "" : forward[forward.length - recallAt]);
  });

  // The messages this page has sent, newest last, so the box can hand one back. Measured in Chrome
  // with a real key press, on the box after a message had gone: `Up` on the empty box left it empty
  // (`value: ""`), while the stub had the message — so the sentence was sent, recorded in the
  // transcript, and the one place it could not be reached again was the box it was typed in. Every
  // agent front end this project is modelled on hands it back, and the reason is ordinary: a
  // message that wants a word changed has to be retyped from nothing without it.
  //
  // Kept here rather than read from the transcript, because the transcript is the server's record of
  // what was *said*, and what this key gives back is what was *typed* — markers and all, so the
  // message can be sent again with the same pictures in it.
  var sentByHand = [];
  var RECALL_KEPT = 50;

  // The messages a person would expect `Up` to walk. Kept short on purpose: a box that remembers the
  // whole session turns one stray arrow key into a page of scrolling.
  function remember(text) {
    var one = String(text).trim();
    if (!one) {
      return;
    }
    // Written twice in a row is one message, not two: the key would otherwise spend two presses
    // returning the same sentence.
    if (sentByHand[sentByHand.length - 1] === one) {
      return;
    }
    sentByHand.push(one);
    if (sentByHand.length > RECALL_KEPT) {
      sentByHand.shift();
    }
  }

  // Which message `Up` is on, counted from the newest: 0 is "not walking", 1 is the last one sent.
  var recallAt = 0;

  // What the conversation already on the server holds, oldest first — the other half of the walk.
  //
  // `sentByHand` alone covers one page's lifetime, and this product reloads the page constantly:
  // the agent changes code, the browser comes back, and the conversation returns because it is read
  // from the file. The messages did not. Measured in Chrome on a page loaded onto a conversation
  // with two exchanges in it — the state every rebuild leaves behind — `Up` three times answered
  // `""`, `""`, `""`, while both exchanges were drawn in the transcript and the three key events
  // had demonstrably arrived. So the one thing a person actually wants after a rebuild, "the same
  // sentence with one word changed", was the one thing the box could not give back.
  var fromTheFile = [];

  // Read out of every history read rather than appended to, so it follows the file: switching
  // conversation, or a turn landing, puts the list back in step instead of accumulating two.
  function seedFromTheFile(turns) {
    var seeded = [];
    for (var i = 0; turns && i < turns.length; i++) {
      var said = String((turns[i] && turns[i].user) || "").trim();
      if (!said) {
        continue;
      }
      // Said twice in a row is one message here too, for the same reason `remember` does it.
      if (seeded[seeded.length - 1] === said) {
        continue;
      }
      seeded.push(said);
    }
    if (seeded.length > RECALL_KEPT) {
      seeded = seeded.slice(seeded.length - RECALL_KEPT);
    }
    fromTheFile = seeded;
  }

  // The walk: the conversation first, then what this page typed.
  //
  // Two lists rather than one, because they arrive in different orders. A message is remembered the
  // moment it leaves the box, and the file only has it after the turn is recorded — so appending
  // both into one list would put an old turn *after* the sentence just sent whenever a history read
  // that started earlier landed later. The file is always older than this page, so the order is
  // not something to reconstruct: it is the shape of the two lists.
  function recallList() {
    if (!fromTheFile.length) {
      return sentByHand;
    }
    if (!sentByHand.length) {
      return fromTheFile;
    }
    // The two meet at a duplicate: by the time the next read lands, the sentence this page sent is
    // in the file as well. One press should not return it twice.
    var joined = fromTheFile.slice();
    for (var i = 0; i < sentByHand.length; i++) {
      if (i === 0 && joined[joined.length - 1] === sentByHand[0]) {
        continue;
      }
      joined.push(sentByHand[i]);
    }
    return joined;
  }

  // The phrase the transcript is being searched for, lowercased once so every comparison is the
  // same comparison. Empty is the ordinary window.
  var finding = "";

  // Typing searches; emptying the field puts the window back. Both go through `readHistory` because
  // the conversation is the server's and this page only draws a view of it — filtering a copy held
  // here would drift from the file the moment a turn landed.
  find.addEventListener("input", function () {
    finding = find.value.trim().toLowerCase();
    readHistory();
  });

  // Escape empties the field and nothing else. The panel closes on Escape through a handler on the
  // document, so without this the one key that means "clear the search" would take the whole panel
  // away instead — and on a long conversation that is the state a person is in when they reach for
  // it.
  find.addEventListener("keydown", function (event) {
    if (event.key !== "Escape") {
      return;
    }
    event.preventDefault();
    event.stopPropagation();
    find.value = "";
    finding = "";
    readHistory();
  });

  // Whether what the box holds right now is a message this key put there and nobody has touched
  // since. Without it the walk stops after one step: the box is no longer empty, so the guard that
  // protects a half-typed sentence would refuse the second `Up` too — measured in Chrome, pressing
  // `Up` twice returned the same message both times and never reached the one before it.
  var recalled = false;

  // A person's own edit ends the walk, so the next `Up` starts again from the newest message rather
  // than from wherever the walk had stopped. `putBack` sets `value` directly, and setting a value
  // from script does not fire this, which is what keeps a recalled message from resetting its own
  // position.
  input.addEventListener("input", function () {
    recallAt = 0;
    recalled = false;
  });

  // Where the caret goes when a message lands in the box from the page rather than from typing.
  // Position 0 is the default after setting `value`, and it would put the next word the person
  // types in front of the sentence they just asked for.
  function caretAtTheEnd() {
    var end = input.value.length;
    try {
      input.setSelectionRange(end, end);
    } catch (error) {
      // A box the browser will not let us place a caret in still holds the text, which is the part
      // that matters.
    }
  }

  // A message put back into the box, with the caret at its end.
  function putBack(text) {
    input.value = text;
    recalled = text !== "";
    input.focus();
    caretAtTheEnd();
    // The markers in a recalled message refer to pictures this page no longer holds, so the note
    // that counts them would be describing attachments that are not there.
    refreshNote();
  }

  // The message back in the box after a turn that could not be sent, with the caret at its end.
  //
  // Not `putBack`: that one records the text as a recalled message, so the next `Up` would replace
  // the sentence it put back. A message whose turn failed is one the person is about to fix, and
  // the history walk takes the arrow keys away from a message being written.
  //
  // Never over their words, either. The box is emptied when the message goes, so a person may have
  // typed the next sentence while the turn was failing; that sentence is theirs and stays.
  function giveBack(text) {
    if (input.value.trim() !== "") {
      return;
    }
    input.value = text;
    caretAtTheEnd();
    refreshNote();
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
    // Remembered before the answer arrives, not after: this is the message that was typed, and a
    // turn that fails still leaves the person wanting it back to fix and resend.
    remember(text);
    // The box empties when the message leaves, not when the answer arrives. Keeping it full for
    // the length of a turn reads as "not sent" — the box is where a person looks to see whether
    // anything happened — and measured in Chrome, the clearing that only ran on the answer deleted
    // a sentence typed during the turn along with it: `value` went from "…and add a total row" to
    // `""` the moment the reply landed. What the turn cost the person is now only what they said.
    // Both are read out of the message, not out of the box, so emptying it here cannot take the
    // pictures and files the words name with it.
    input.value = "";
    refreshNote();
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
        if (wantsEvents(response)) {
          // Both readings end in the same place: the last event of a stream carries the reply the
          // JSON path would have handed over whole, so everything below this line is unchanged
          // whichever way the answer arrived.
          return readEvents(response, typeMore).then(function (body) {
            return { status: response.status, body: body, streamed: true };
          });
        }
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
          forgetAttachments();
          // The answer does not come through here: it is in the file before the route answers, so
          // the reading below draws it from the record, once, in the exchange it belongs to. What
          // this page has to say about the turn is the two things the record cannot know.
          if (result.body.cancelled) {
            // The note the loop writes says the turn was cancelled; it cannot say by whom. This
            // page knows, and the difference is the whole reading: a turn that stopped itself is
            // something to look into, and one the person ended is the page working.
            say(
              "Stopped at your request, after " +
                Math.round((Date.now() - sentAt) / 1000) +
                "s.",
              false
            );
          }
          if (result.body.changed) {
            // The turn came back as soon as it had written; the build it started is still running.
            // Saying "reload to see it" was the whole defect — nothing had been built to see — so
            // this names what is happening and `watchTheRebuild` finishes the sentence.
            say("This changed the app — rebuilding it now.", false);
          }
          // What it cost, read from the reply rather than from the transcript: the transcript is the
          // conversation and knows nothing about money, and this is the one row about the turn
          // rather than about what was said in it.
          showSpend(result.body);
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
          // The page never had the message, so the box keeps it: the reload this line asks for
          // would otherwise take the sentence with it.
          giveBack(text);
          return;
        }
        if (result.streamed) {
          // A stream that ended without its closing event: the connection went away mid-answer, or
          // the page was closed on the other end. The turn is not necessarily over — the server
          // keeps running it — so this says what happened to the *reading*, not to the turn, and
          // points at the transcript, which is where the turn will land either way.
          say(
            "The answer stopped partway through — the connection to deepnest closed. Reload the " +
              "page to read what the turn recorded.",
            true
          );
          readHistory();
          // Not given back, unlike the refusals above: this turn did run, so a box holding the
          // message again is an invitation to ask it a second time. `Up` still has it — the recall
          // list is written when the message leaves, and it is the one place a sent message stays.
          return;
        }
        var why =
          result.body && typeof result.body.error === "string"
            ? result.body.error
            : "the request failed (" + result.status + ")";
        say(why, true);
        giveBack(text);
      })
      .catch(function () {
        send.disabled = false;
        stopPolling();
        say("The request did not reach deepnest. Is it still running?", true);
        giveBack(text);
      });
  });

  stop.addEventListener("click", function () {
    if (!slug) {
      return;
    }
    // Said here, before the request leaves. Waiting for the reply to say it is what made a stop
    // look inert: the round trip is quick, but the wait this has to cover is the model call the
    // turn is already inside, and for those seconds the page said nothing at all.
    stopping = true;
    stop.disabled = true;
    stop.textContent = STOPPING_LABEL;
    showStopping(
      "Stopping \u2014 a running command is killed now; a model call already sent ends when it " +
        "returns."
    );
    // No body, and that is the whole of the bug this line carried: the route refuses a request
    // body by name — a server must not answer "done" about content it never read — so `body: "{}"`
    // turned every press of this control into a `400` and the flag was never set. Measured on the
    // running page: a turn went on spending after the press, and the only thing that changed was
    // the wording above it.
    fetch(stoppingRoute(), {
      method: "POST",
      credentials: "same-origin"
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
        if (result.status === 200 && result.body && typeof result.body.note === "string") {
          // The turn is not over yet — the loop reads the flag before its next request and before
          // every tool call of a batch — so this says what was asked rather than what has
          // happened. The control stays as it is, disabled and reading "Stopping…": the turn is
          // still running, and the only thing a second press could do is ask again.
          showStopping("Stopping \u2014 " + result.body.note);
          return;
        }
        // A stop that did not arrive is not a stop: the turn is still spending, so the control
        // goes back to being one a person can press.
        stopping = false;
        stop.disabled = false;
        stop.textContent = STOP_LABEL;
        if (result.status === 401) {
          say("This page's permission to stop has expired — reload the page and try again.", true);
          return;
        }
        say("The stop did not reach deepnest (" + result.status + ").", true);
      })
      .catch(function () {
        stopping = false;
        stop.disabled = false;
        stop.textContent = STOP_LABEL;
        say("The stop did not reach deepnest. Is it still running?", true);
      });
  });

  allow.addEventListener("click", function () {
    answerTheQuestion(true);
  });

  refuse.addEventListener("click", function () {
    answerTheQuestion(false);
  });

  jump.addEventListener("click", function () {
    transcript.scrollTop = transcript.scrollHeight;
    offerTheLatest(atTheLatest());
  });

  ball.addEventListener("click", function () {
    show(panel.hidden);
  });

  // The app's frame, as far as this page is concerned.
  //
  // A frame is a document boundary, and two things this dialog had while it stood in the app's own
  // page are on the wrong side of it: the chord that opens the panel, and the click in the app that
  // closes it. Neither crosses, so the frame is handed a script that forwards exactly those two —
  // added from here on every load, because the app's page belongs to the agent that writes it and
  // this is the one thing about it deepnest keeps.
  //
  // The title travels the other way. The tab is named after the app, and the app's own document is
  // the only place that knows what it is called.
  if (stage !== null) {
    stage.addEventListener("load", adoptTheFrame);
    window.addEventListener("message", function (event) {
      // The frame is same-origin, and this is what says so. A message from anywhere else is not a
      // frame of ours being helpful.
      if (event.origin !== window.location.origin) {
        return;
      }
      var said = event.data;
      if (!said || said.deepnest !== "frame") {
        return;
      }
      if (said.kind === "chord") {
        show(true);
      } else if (said.kind === "press" && !panel.hidden) {
        // Everything in the app is outside a panel drawn over it — the rule this page's own click
        // handler applies to the page's contents, applied to the document behind them.
        show(false);
      }
    });
    adoptTheFrame();
  }

  // What this page keeps of the frame's own document: its name, and the one script that lets two
  // things cross the boundary.
  function adoptTheFrame() {
    var app = null;
    try {
      app = stage.contentDocument;
    } catch (error) {
      app = null;
    }
    if (!app || !app.documentElement) {
      return;
    }
    // The app's document is the only place that knows what the app is called, and the frame has
    // finished loading by the time this is called: its <title> is read here, once, and written onto
    // the tab. The stage's own name is a placeholder slug until this line runs.
    if (app.title) {
      document.title = app.title;
    }
    // Once per document: a reload brings a new one, and a second copy would forward every chord
    // twice — which for the chord is invisible, and for a click is a panel that closes and opens.
    if (app.getElementById("deepnest-dialog-frame")) {
      return;
    }
    var forwarder = app.createElement("script");
    forwarder.id = "deepnest-dialog-frame";
    forwarder.src = "/dialog-frame.js";
    (app.head || app.documentElement).appendChild(forwarder);
  }

  // The page a finished rebuild reloads onto has no memory of the turn that started it, so this
  // is where the conversation comes back from: the file, not the tab.
  readHistory();

  // The way in that does not start at the top of the app's page.
  //
  // The dialog's root is appended to `body` last, so the ball is the *last* tab stop in the
  // document: measured in Chrome through the browser's own key pipeline, reaching it took seven
  // presses of Tab on a page with six app controls and twenty-nine on one with twenty-eight — the
  // app's own count, plus one, every time. Everything after that worked: Enter on the ball opened
  // the panel and put the caret in the message box, Escape closed it and gave the ball its focus
  // back. Walking the app was the whole of the difficulty, so this is a way past it rather than a
  // repair of anything downstream.
  //
  // `Cmd`/`Ctrl`+`K` rather than a bare letter: the app owns its page, and a single key would take
  // typing away from every field in it. It is what the agent products this dialog follows put the
  // conversation on, so it is the chord a person is likely to try first.
  //
  // In the bubble phase, and behind `defaultPrevented`: an app that binds the same chord for a
  // command palette of its own gets the first word and keeps it, because its handler runs first
  // and `preventDefault` is how it says so. Taking a chord away from the page this is injected
  // into is worse than missing one — Escape above is confined the same way, to a panel that is
  // actually open, so a closed dialog claims no key at all.
  //
  // `isComposing` is the other guard: an input method turns a `k` into half a character while it
  // is composing, and the panel opening on that would eat the one letter the person was typing.
  document.addEventListener("keydown", function (event) {
    if (event.key.toLowerCase() !== "k") {
      return;
    }
    if (!event.metaKey && !event.ctrlKey) {
      return;
    }
    if (event.altKey || event.shiftKey || event.defaultPrevented || event.isComposing) {
      return;
    }
    // One meaning for the chord whether the panel is open or shut: the caret belongs in the
    // message box. Opening it only when it was shut would make a second press do nothing at all,
    // and a dialog the person can already see is still three Tab stops from its own input.
    event.preventDefault();
    show(true);
  });

  document.addEventListener("keydown", function (event) {
    if (event.key === "Escape" && !panel.hidden) {
      // One level at a time. On an expanded panel the step being undone is the expansion — the
      // person asked for a bigger window and is backing out of that — so the next press is the one
      // that closes the dialog. This is the same layering the screenshot sheet already uses.
      if (expanded) {
        setExpanded(false);
        return;
      }
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
