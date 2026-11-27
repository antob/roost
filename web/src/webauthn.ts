// webauthn-rs speaks base64url JSON; the browser API wants ArrayBuffers.

function fromB64url(s: string): ArrayBuffer {
  const b64 = s.replace(/-/g, "+").replace(/_/g, "/").padEnd(Math.ceil(s.length / 4) * 4, "=");
  const bin = atob(b64);
  const out = new Uint8Array(bin.length);
  for (let i = 0; i < bin.length; i++) out[i] = bin.charCodeAt(i);
  return out.buffer;
}

function toB64url(buf: ArrayBuffer | null | undefined): string | undefined {
  if (!buf) return undefined;
  let bin = "";
  for (const b of new Uint8Array(buf)) bin += String.fromCharCode(b);
  return btoa(bin).replace(/\+/g, "-").replace(/\//g, "_").replace(/=+$/, "");
}

export async function createPasskey(options: any): Promise<unknown> {
  const pk = options.publicKey;
  pk.challenge = fromB64url(pk.challenge);
  pk.user.id = fromB64url(pk.user.id);
  for (const c of pk.excludeCredentials ?? []) c.id = fromB64url(c.id);
  const cred = (await navigator.credentials.create({ publicKey: pk })) as PublicKeyCredential;
  const res = cred.response as AuthenticatorAttestationResponse;
  return {
    id: cred.id,
    rawId: toB64url(cred.rawId),
    type: cred.type,
    response: {
      attestationObject: toB64url(res.attestationObject),
      clientDataJSON: toB64url(res.clientDataJSON),
      transports: res.getTransports?.() ?? [],
    },
    extensions: cred.getClientExtensionResults(),
  };
}

export async function assertPasskey(options: any): Promise<unknown> {
  const pk = options.publicKey;
  pk.challenge = fromB64url(pk.challenge);
  for (const c of pk.allowCredentials ?? []) c.id = fromB64url(c.id);
  const cred = (await navigator.credentials.get({ publicKey: pk })) as PublicKeyCredential;
  const res = cred.response as AuthenticatorAssertionResponse;
  return {
    id: cred.id,
    rawId: toB64url(cred.rawId),
    type: cred.type,
    response: {
      authenticatorData: toB64url(res.authenticatorData),
      clientDataJSON: toB64url(res.clientDataJSON),
      signature: toB64url(res.signature),
      userHandle: toB64url(res.userHandle),
    },
    extensions: cred.getClientExtensionResults(),
  };
}

export function b64urlToUint8(s: string): Uint8Array {
  return new Uint8Array(fromB64url(s));
}
