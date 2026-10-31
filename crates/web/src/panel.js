// The panel's script. No framework and no build step: this file is served as written, so what a
// user's browser runs is what is in the repository.
//
// Two rules hold throughout, and both are about the fact that the strings this page renders come
// from the filesystem — an app's directory name, an error the server wrote:
//
// 1. Text from anywhere is inserted with `textContent`, never `innerHTML`. The policy forbids inline
//    script, so a string cannot execute on its own; `textContent` is what stops it from becoming
//    markup that a later mistake could act on.
// 2. The control token is read from the DOM and sent as a bearer credential. It is never put in a
//    URL, so it never lands in the browser's history or in a `Referer`.

(function () {
  "use strict";

  function meta(name) {
    var element = document.querySelector('meta[name="' + name + '"]');
    return element ? element.getAttribute("content") : "";
  }

  var TOKEN = meta("deepnest-token");
  var PORT = meta("deepnest-port");

  var status = document.getElementById("status");
  var endpointLine = document.getElementById("endpoint");
  var firstRun = document.getElementById("first-run");
  var appsBox = document.getElementById("apps");

  // One place the answer is parsed, so a refusal the server wrote reaches the user as text rather
  // than as "[object Object]" or as silence.
  function api(path, options) {
    options = options || {};
    options.headers = options.headers || {};
    options.headers["Authorization"] = "Bearer " + TOKEN;
    if (options.body) {
      options.headers["Content-Type"] = "application/json";
    }
    options.mode = "same-origin";
    options.cache = "no-store";
    return fetch(path, options).then(function (response) {
      return response.text().then(function (text) {
        var body = null;
        if (text) {
          try {
            body = JSON.parse(text);
          } catch (error) {
            body = { error: text };
          }
        }
        if (!response.ok) {
          var reason =
            body && (body.error || body.detail)
              ? body.error || body.detail
              : response.status + " " + response.statusText;
          throw new Error(reason);
        }
        return body;
      });
    });
  }

  function say(text, kind) {
    status.textContent = text || "";
    status.className = "note" + (kind ? " " + kind : "");
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

  function button(label, onClick) {
    var node = element("button", "", label);
    node.type = "button";
    node.addEventListener("click", function () {
      node.disabled = true;
      Promise.resolve(onClick()).then(
        function () {
          node.disabled = false;
        },
        function (error) {
          node.disabled = false;
          say(String(error.message || error), "bad");
        }
      );
    });
    return node;
  }

  // ---- the model endpoint -------------------------------------------------------------------

  function renderEndpoint(state) {
    endpointLine.textContent = "";
    if (state && state.configured) {
      endpointLine.appendChild(document.createTextNode("Model endpoint: configured ("));
      endpointLine.appendChild(element("code", "", state.model || "unnamed model"));
      endpointLine.appendChild(document.createTextNode(")."));
      firstRun.hidden = true;
    } else {
      endpointLine.appendChild(document.createTextNode("Model endpoint: "));
      endpointLine.appendChild(element("strong", "bad", "not configured"));
      endpointLine.appendChild(
        document.createTextNode(" — deepnest needs one before it can write anything.")
      );
      firstRun.hidden = false;
    }
  }

  function refreshEndpoint() {
    // `/api/config` rather than `/api/health`: the health probe carries no user data and therefore
    // names no model, which is exactly why it can be read without the token.
    return api("/api/config").then(renderEndpoint);
  }

  document.getElementById("endpoint-form").addEventListener("submit", function (event) {
    event.preventDefault();
    var payload = {
      base_url: document.getElementById("base-url").value.trim(),
      api_key: document.getElementById("api-key").value,
      model: document.getElementById("model").value.trim()
    };
    say("Saving the endpoint…");
    api("/api/config", { method: "POST", body: JSON.stringify(payload) })
      .then(function (saved) {
        // The key's own field is cleared: it is in `config.toml` now, and a password input that
        // still holds it is one screenshot away from being a leak.
        document.getElementById("api-key").value = "";
        say("Saved. deepnest is using it now.", "good");
        return renderEndpoint(saved);
      })
      .catch(function (error) {
        say("Could not save: " + (error.message || error), "bad");
      });
  });

  // ---- the app list -------------------------------------------------------------------------

  function renderApp(app) {
    var card = element("div", "app");
    var head = element("div", "row");
    head.appendChild(element("h3", "", app.name || app.slug));
    head.appendChild(element("span", "slug", app.slug));
    if (app.template) {
      head.appendChild(element("span", "slug", app.template));
    }
    card.appendChild(head);
    if (app.path) {
      card.appendChild(element("div", "note mono", app.path));
    }

    var actions = element("div", "actions");
    var open = element("a", "", "Open");
    open.href = "/app/" + encodeURIComponent(app.slug) + "/";
    open.target = "_blank";
    open.rel = "noopener";
    actions.appendChild(open);

    // The four verbs the API has. Each names the route it calls, so a failure reports which one
    // failed rather than "something went wrong".
    ["build", "start", "stop", "restart"].forEach(function (action) {
      actions.appendChild(
        button(action, function () {
          say("Requesting " + action + " for " + app.slug + "…");
          var path = "/api/apps/" + encodeURIComponent(app.slug) + "/" + action;
          return api(path, { method: "POST" }).then(
            function (answer) {
              var state = answer && answer.state ? answer.state : "ok";
              say(app.slug + ": " + action + " answered " + state, "good");
              return refreshApps();
            },
            function (error) {
              throw new Error(app.slug + ": " + (error.message || error));
            }
          );
        })
      );
    });
    card.appendChild(actions);
    return card;
  }

  function renderApps(listing) {
    appsBox.textContent = "";
    var apps = (listing && listing.apps) || [];
    if (!apps.length) {
      appsBox.appendChild(element("p", "note", "No apps yet. Create one below."));
    }
    apps.forEach(function (app) {
      appsBox.appendChild(renderApp(app));
    });

    // Directories that are not apps are reported by path and reason: an app that failed to parse
    // must not look like an app that does not exist.
    var skipped = (listing && listing.skipped) || [];
    if (skipped.length) {
      var box = element("div", "app");
      box.appendChild(element("h3", "bad", "Not usable as apps"));
      skipped.forEach(function (entry) {
        box.appendChild(element("p", "note", entry.path + " — " + entry.reason));
      });
      appsBox.appendChild(box);
    }
  }

  function refreshApps() {
    return api("/api/apps").then(renderApps);
  }

  document.getElementById("create-form").addEventListener("submit", function (event) {
    event.preventDefault();
    var slug = document.getElementById("new-slug").value.trim();
    var name = document.getElementById("new-name").value.trim();
    say("Creating " + slug + "…");
    var path = "/api/apps/" + encodeURIComponent(slug) + "/create?name=" + encodeURIComponent(name);
    api(path, { method: "POST" })
      .then(function (created) {
        say("Created " + created.slug + " in " + created.dir, "good");
        document.getElementById("new-slug").value = "";
        document.getElementById("new-name").value = "";
        return refreshApps();
      })
      .catch(function (error) {
        say("Could not create: " + (error.message || error), "bad");
      });
  });

  // ---- start ---------------------------------------------------------------------------------

  // Health first: it answers without the token, so it is the one call that separates "deepnest is
  // not answering" from "the token was refused".
  api("/api/health")
    .then(refreshEndpoint)
    .catch(function (error) {
      say("Cannot reach deepnest on 127.0.0.1:" + PORT + " — " + (error.message || error), "bad");
    });

  refreshApps().catch(function (error) {
    appsBox.textContent = "";
    appsBox.appendChild(element("p", "bad", "Cannot read the app list: " + (error.message || error)));
  });

  // ---- skill updates -------------------------------------------------------------------------

  // Asked for after the page is already readable, and never rendered into the markup: this is one
  // request per installed skill to somebody else's server, so on a machine with no network it takes
  // until that request fails. A page that waited for it would hide the app list — and stopping a
  // runaway app — behind a question about updates.
  function refreshUpdates() {
    var section = document.getElementById("skills-section");
    if (!section) {
      return;
    }
    api("/api/skills/updates")
      .then(function (report) {
        var lines = [];
        // The rows a person can act on: a published release, or a skill whose installed version
        // nobody recorded. Each gets its own button below, because a refresh runs a program on this
        // machine and one press that ran several would act on a set the user never saw.
        var actionable = [];
        (report.skills || []).forEach(function (skill) {
          if (skill.state === "newer") {
            lines.push(skill.name + ": release " + skill.version + " is published.");
            actionable.push(skill);
          } else if (skill.state === "unknown") {
            lines.push(
              skill.name + ": " + skill.version + " is published; no installed version is recorded, " +
                "so there is nothing to compare it with yet."
            );
            actionable.push(skill);
          } else if (skill.state === "unchecked") {
            // Said out loud rather than left blank: a missing line and "up to date" would look the
            // same, which is the reading the four-state check exists to prevent.
            lines.push(skill.name + ": could not check — " + (skill.detail || "no reason given") + ".");
          }
        });
        if (!lines.length) {
          return;
        }
        var note = element("p", "note", lines.join(" "));
        note.id = "skills-updates";
        section.appendChild(note);
        actionable.forEach(function (skill) {
          if (document.getElementById("refresh-" + skill.name)) {
            return;
          }
          var row = element("p", "note");
          row.id = "refresh-" + skill.name;
          var press = button("Refresh " + skill.name, function () {
            return askRefresh(skill.name, row, press);
          });
          row.appendChild(press);
          section.appendChild(row);
        });
      })
      .catch(function () {
        // The check is a courtesy, and a page that reported its failure in red would be shouting
        // about the least important thing on it. The skills themselves are already listed above,
        // read from disk with no network at all.
      });
  }

  // ---- skill refresh -------------------------------------------------------------------------

  // A refresh runs the package that publishes the skill, so it is one explicit press per skill, and
  // the outcome arrives through the same route: the server starts it and answers at once, because
  // the publisher returns when that program decides to — longer than a browser holds a request.
  var REFRESH_POLL_MS = 500;
  var REFRESH_POLLS = 240;

  function askRefresh(name, row, press) {
    press.textContent = "Refreshing " + name + "...";
    return api("/api/skills/refresh", { method: "POST", body: JSON.stringify({ name: name }) })
      .then(function () {
        return awaitRefresh(name, row, REFRESH_POLLS);
      })
      .then(function () {
        // Enabled again either way: a refresh that failed can be asked for again, and one that
        // worked is cheap to repeat.
        press.textContent = "Refresh " + name;
      })
      .catch(function (error) {
        press.textContent = "Refresh " + name;
        throw error;
      });
  }

  function awaitRefresh(name, row, left) {
    return api("/api/skills/refresh").then(function (answer) {
      var refresh = answer && answer.refresh;
      if (refresh && refresh.name === name && refresh.state !== "refreshing") {
        if (refresh.state === "refreshed") {
          reportRefresh(
            row,
            "refreshed" + (refresh.version ? " to " + refresh.version : ", version still unknown"),
            "note"
          );
        } else {
          reportRefresh(row, "not refreshed: " + (refresh.detail || "no reason given"), "bad");
        }
        return null;
      }
      if (left <= 0) {
        reportRefresh(row, "still running; reload the page to see how it ended", "note");
        return null;
      }
      return new Promise(function (resolve) {
        setTimeout(function () {
          resolve(awaitRefresh(name, row, left - 1));
        }, REFRESH_POLL_MS);
      });
    });
  }

  // Replaced rather than appended: a second press must not leave the previous answer standing
  // beside the new one.
  function reportRefresh(row, text, kind) {
    var said = row.querySelector(".refresh-outcome");
    if (!said) {
      said = element("span", "");
      row.appendChild(said);
    }
    said.className = kind + " refresh-outcome";
    said.textContent = " " + text;
  }

  refreshUpdates();
})();
