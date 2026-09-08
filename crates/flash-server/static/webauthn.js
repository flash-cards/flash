// WebAuthn ceremony glue: converts between the server's JSON challenge
// format (base64url) and the browser's ArrayBuffer credential API.
(function () {
  "use strict";

  function b64uToBuf(s) {
    s = s.replace(/-/g, "+").replace(/_/g, "/");
    const pad = s.length % 4 ? "=".repeat(4 - (s.length % 4)) : "";
    const bin = atob(s + pad);
    const buf = new Uint8Array(bin.length);
    for (let i = 0; i < bin.length; i++) buf[i] = bin.charCodeAt(i);
    return buf.buffer;
  }

  function bufToB64u(buf) {
    const bytes = new Uint8Array(buf);
    let bin = "";
    for (const b of bytes) bin += String.fromCharCode(b);
    return btoa(bin).replace(/\+/g, "-").replace(/\//g, "_").replace(/=+$/, "");
  }

  async function post(url, body) {
    const res = await fetch(url, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: body === undefined ? undefined : JSON.stringify(body),
    });
    if (!res.ok) throw new Error(await res.text());
    return res.json();
  }

  function showError(err) {
    const el = document.getElementById("error");
    if (el) el.textContent = err.name === "NotAllowedError" ? "Cancelled or timed out." : String(err.message || err);
  }

  // Buttons are wired here (not inline handlers) because the CSP forbids
  // inline script.
  document.addEventListener("DOMContentLoaded", function () {
    const enroll = document.getElementById("enroll-btn");
    if (enroll) enroll.addEventListener("click", () => flashEnroll(enroll.dataset.token, enroll.dataset.next || "/"));
    const login = document.getElementById("login-btn");
    if (login) login.addEventListener("click", () => flashLogin(login.dataset.next || "/"));
  });

  window.flashEnroll = async function (token, next) {
    const email = document.getElementById("email");
    if (email && !email.reportValidity()) return;
    try {
      const challenge = await post("/auth/enroll/" + token + "/start", {
        email: email ? email.value : "",
        next: next || "/",
      });
      const pk = challenge.publicKey;
      pk.challenge = b64uToBuf(pk.challenge);
      pk.user.id = b64uToBuf(pk.user.id);
      if (pk.excludeCredentials)
        pk.excludeCredentials = pk.excludeCredentials.map((c) => ({ ...c, id: b64uToBuf(c.id) }));
      const cred = await navigator.credentials.create({ publicKey: pk });
      const out = await post("/auth/enroll/finish", {
        id: cred.id,
        rawId: bufToB64u(cred.rawId),
        type: cred.type,
        extensions: cred.getClientExtensionResults(),
        response: {
          attestationObject: bufToB64u(cred.response.attestationObject),
          clientDataJSON: bufToB64u(cred.response.clientDataJSON),
        },
      });
      window.location.href = out.next || "/";
    } catch (err) {
      showError(err);
    }
  };

  window.flashLogin = async function (next) {
    try {
      const challenge = await post("/auth/login/start");
      const pk = challenge.publicKey;
      pk.challenge = b64uToBuf(pk.challenge);
      if (pk.allowCredentials)
        pk.allowCredentials = pk.allowCredentials.map((c) => ({ ...c, id: b64uToBuf(c.id) }));
      const cred = await navigator.credentials.get({ publicKey: pk });
      await post("/auth/login/finish", {
        id: cred.id,
        rawId: bufToB64u(cred.rawId),
        type: cred.type,
        extensions: cred.getClientExtensionResults(),
        response: {
          authenticatorData: bufToB64u(cred.response.authenticatorData),
          clientDataJSON: bufToB64u(cred.response.clientDataJSON),
          signature: bufToB64u(cred.response.signature),
          userHandle: cred.response.userHandle ? bufToB64u(cred.response.userHandle) : null,
        },
      });
      window.location.href = next || "/";
    } catch (err) {
      showError(err);
    }
  };
})();
