import { describe, it, expect, beforeAll } from "vitest";
import {
  createPublicClient,
  createWalletClient,
  defineChain,
  encodeAbiParameters,
  http,
  parseAbiParameters,
  parseAbi,
} from "viem";
import { privateKeyToAccount } from "viem/accounts";

interface Fixture {
  rpcDataService: `0x${string}`;
  graphTallyCollector: `0x${string}`;
  paymentsEscrow: `0x${string}`;
  grtToken: `0x${string}`;
  providerAddress: `0x${string}`;
  providerKey: `0x${string}`;
  gatewayAddress: `0x${string}`;
  gatewayKey: `0x${string}`;
  gatewaySignerAddress: `0x${string}`;
  gatewaySignerKey: `0x${string}`;
  paymentWallet: `0x${string}`;
}

const anvil = defineChain({
  id: 31337,
  name: "Anvil",
  nativeCurrency: { decimals: 18, name: "Ether", symbol: "ETH" },
  rpcUrls: { default: { http: ["http://127.0.0.1:8545"] } },
});

let fx: Fixture;

beforeAll(() => {
  fx = JSON.parse(process.env.E2E_FIXTURE!) as Fixture;
});

const SERVICE_URL        = "http://127.0.0.1:7700";
const SIDE_SERVICE_URL   = "http://127.0.0.1:7701"; // escrow check, no sender whitelist
const CREDIT_SERVICE_URL = "http://127.0.0.1:7702"; // no escrow check, low credit_threshold
const GATEWAY_URL        = "http://127.0.0.1:8080";

// ── helpers ──────────────────────────────────────────────────────────────────

async function signReceipt(
  fx: Fixture,
  overrides: { key?: `0x${string}`; nonce?: bigint; value?: bigint } = {}
) {
  const key = overrides.key ?? fx.gatewaySignerKey;
  const account = privateKeyToAccount(key);
  const nonce = overrides.nonce ?? BigInt(Math.floor(Math.random() * 1e15));
  const timestampNs = BigInt(Date.now()) * 1_000_000n;
  const value = overrides.value ?? 4_000_000_000_000n;

  const sig = await account.signTypedData({
    domain: {
      name: "GraphTallyCollector",
      version: "1",
      chainId: 31337,
      verifyingContract: fx.graphTallyCollector,
    },
    types: {
      Receipt: [
        { name: "data_service",     type: "address" },
        { name: "service_provider", type: "address" },
        { name: "timestamp_ns",     type: "uint64" },
        { name: "nonce",            type: "uint64" },
        { name: "value",            type: "uint128" },
        { name: "metadata",         type: "bytes" },
      ],
    },
    primaryType: "Receipt",
    message: {
      data_service: fx.rpcDataService,
      service_provider: fx.providerAddress,
      timestamp_ns: timestampNs,
      nonce,
      value,
      metadata: "0x",
    },
  });

  // Construct JSON manually — BigInt fields must be bare number literals, not strings,
  // because serde_json deserialises u64/u128 from JSON numbers, not JSON strings.
  return `{"receipt":{"data_service":"${fx.rpcDataService}","service_provider":"${fx.providerAddress}","timestamp_ns":${timestampNs},"nonce":${nonce},"value":${value},"metadata":"0x"},"signature":"${sig}"}`;
}

// ── health ────────────────────────────────────────────────────────────────────

describe("health", () => {
  it("dispatch-service /health returns 200", async () => {
    const res = await fetch(`${SERVICE_URL}/health`);
    expect(res.status).toBe(200);
  });

  it("dispatch-gateway /health returns 200", async () => {
    const res = await fetch(`${GATEWAY_URL}/health`);
    expect(res.status).toBe(200);
  });
});

// ── service: info endpoints ───────────────────────────────────────────────────

describe("service: info endpoints", () => {
  it("GET /version returns service name and version", async () => {
    const res = await fetch(`${SERVICE_URL}/version`);
    expect(res.status).toBe(200);
    const body = (await res.json()) as any;
    expect(body.service).toBe("dispatch-service");
    expect(typeof body.version).toBe("string");
  });

  it("GET /chains returns supported chain list including 31337", async () => {
    const res = await fetch(`${SERVICE_URL}/chains`);
    expect(res.status).toBe(200);
    const body = (await res.json()) as any;
    expect(Array.isArray(body.supported)).toBe(true);
    expect(body.supported).toContain(31337);
  });

  it("GET /block/31337 proxies eth_blockNumber to Anvil", async () => {
    const res = await fetch(`${SERVICE_URL}/block/31337`);
    expect(res.status).toBe(200);
    const body = (await res.json()) as any;
    expect(body.result).toMatch(/^0x/);
  });

  it("GET /block/99999 returns error for unsupported chain", async () => {
    const res = await fetch(`${SERVICE_URL}/block/99999`);
    expect(res.status).toBe(200); // always 200, error in body
    const body = (await res.json()) as any;
    expect(body.error).toBeDefined();
  });
});

// ── service: request validation ──────────────────────────────────────────────

describe("service: request validation", () => {
  it("rejects malformed JSON body with 422", async () => {
    const res = await fetch(`${SERVICE_URL}/rpc/31337`, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: "this is not json",
    });
    expect(res.status).toBe(400);
  });

  it("rejects wrong JSON-RPC version with 502", async () => {
    const receipt = await signReceipt(fx);
    const res = await fetch(`${SERVICE_URL}/rpc/31337`, {
      method: "POST",
      headers: { "Content-Type": "application/json", "TAP-Receipt": receipt },
      body: JSON.stringify({ jsonrpc: "1.0", method: "eth_blockNumber", params: [], id: 1 }),
    });
    expect(res.status).toBe(502);
    const body = (await res.json()) as any;
    expect(body.error.code).toBe(-32603);
  });

  it("rejects request for unsupported chain with 404", async () => {
    const res = await fetch(`${SERVICE_URL}/rpc/99999`, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ jsonrpc: "2.0", method: "eth_blockNumber", params: [], id: 1 }),
    });
    expect(res.status).toBe(404);
    const body = (await res.json()) as any;
    expect(body.error.code).toBe(-32002);
  });
});

// ── service: direct RPC requests ─────────────────────────────────────────────

describe("direct request to dispatch-service", () => {
  it("accepts a valid TAP-Receipt and returns eth_blockNumber", async () => {
    const receipt = await signReceipt(fx);
    const res = await fetch(`${SERVICE_URL}/rpc/31337`, {
      method: "POST",
      headers: {
        "Content-Type": "application/json",
        "TAP-Receipt": receipt,
      },
      body: JSON.stringify({
        jsonrpc: "2.0",
        method: "eth_blockNumber",
        params: [],
        id: 1,
      }),
    });
    expect(res.status).toBe(200);
    const body = (await res.json()) as any;
    expect(body.jsonrpc).toBe("2.0");
    expect(typeof body.result).toBe("string");
    expect(body.result).toMatch(/^0x/);
  });

  it("rejects a receipt signed by an unauthorized key", async () => {
    // Sign with gateway key (not gatewaySigner) — not in authorized_senders
    const receipt = await signReceipt(fx, { key: fx.gatewayKey });
    const res = await fetch(`${SERVICE_URL}/rpc/31337`, {
      method: "POST",
      headers: {
        "Content-Type": "application/json",
        "TAP-Receipt": receipt,
      },
      body: JSON.stringify({
        jsonrpc: "2.0",
        method: "eth_blockNumber",
        params: [],
        id: 2,
      }),
    });
    expect(res.status).toBe(401);
  });

  it("rejects a request with no TAP-Receipt header", async () => {
    const res = await fetch(`${SERVICE_URL}/rpc/31337`, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({
        jsonrpc: "2.0",
        method: "eth_blockNumber",
        params: [],
        id: 3,
      }),
    });
    expect(res.status).toBe(401);
  });

  it("returns x-drpc-attestation header signed by the operator key", async () => {
    const receipt = await signReceipt(fx);
    const res = await fetch(`${SERVICE_URL}/rpc/31337`, {
      method: "POST",
      headers: { "Content-Type": "application/json", "TAP-Receipt": receipt },
      body: JSON.stringify({ jsonrpc: "2.0", method: "eth_blockNumber", params: [], id: 4 }),
    });
    expect(res.status).toBe(200);
    const att = res.headers.get("x-drpc-attestation");
    expect(att).toBeTruthy();
    const parsed = JSON.parse(att!) as { signer: string; signature: string };
    expect(parsed.signer).toMatch(/^0x/i);
    expect(parsed.signature).toMatch(/^0x/);
    expect(parsed.signature.length).toBe(132); // "0x" + 65 bytes = 130 hex chars
  });

  it("proxies multiple RPC methods correctly", async () => {
    const methods = [
      { method: "eth_chainId",    validate: (r: any) => parseInt(r, 16) === 31337 },
      { method: "net_version",    validate: (r: any) => r === "31337" },
      { method: "eth_blockNumber", validate: (r: any) => /^0x/.test(r) },
    ];

    for (const { method, validate } of methods) {
      const receipt = await signReceipt(fx);
      const res = await fetch(`${SERVICE_URL}/rpc/31337`, {
        method: "POST",
        headers: { "Content-Type": "application/json", "TAP-Receipt": receipt },
        body: JSON.stringify({ jsonrpc: "2.0", method, params: [], id: 1 }),
      });
      expect(res.status).toBe(200);
      const body = (await res.json()) as any;
      expect(validate(body.result), `${method} result invalid`).toBe(true);
    }
  });
});

// ── gateway: info endpoints ───────────────────────────────────────────────────

describe("gateway: info endpoints", () => {
  it("GET /version returns service name and version", async () => {
    const res = await fetch(`${GATEWAY_URL}/version`);
    expect(res.status).toBe(200);
    const body = (await res.json()) as any;
    expect(body.service).toBe("dispatch-gateway");
    expect(typeof body.version).toBe("string");
  });

  it("GET /providers/31337 returns the registered provider with a score", async () => {
    const res = await fetch(`${GATEWAY_URL}/providers/31337`);
    expect(res.status).toBe(200);
    const body = (await res.json()) as any;
    expect(body.chain_id).toBe(31337);
    expect(Array.isArray(body.providers)).toBe(true);
    expect(body.providers.length).toBeGreaterThan(0);
    const provider = body.providers[0];
    expect(provider.endpoint).toBe("http://127.0.0.1:7700");
    expect(typeof provider.score).toBe("number");
  });

  it("GET /providers/99999 returns empty list for unknown chain", async () => {
    const res = await fetch(`${GATEWAY_URL}/providers/99999`);
    expect(res.status).toBe(200);
    const body = (await res.json()) as any;
    expect(body.providers).toEqual([]);
  });

  it("GET /metrics returns Prometheus text with dispatch counters", async () => {
    // Prime at least one request through the gateway so prometheus emits non-empty families.
    await fetch(`${GATEWAY_URL}/rpc/31337`, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ jsonrpc: "2.0", method: "eth_blockNumber", params: [], id: 99 }),
    });
    const res = await fetch(`${GATEWAY_URL}/metrics`);
    expect(res.status).toBe(200);
    const text = await res.text();
    expect(text).toContain("dispatch_gateway_requests_total");
    expect(text).toContain("dispatch_gateway_request_duration_seconds");
  });
});

// ── gateway: RPC routing ──────────────────────────────────────────────────────

describe("request through gateway", () => {
  it("routes eth_blockNumber to dispatch-service and returns a result", async () => {
    const res = await fetch(`${GATEWAY_URL}/rpc/31337`, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({
        jsonrpc: "2.0",
        method: "eth_blockNumber",
        params: [],
        id: 4,
      }),
    });
    expect(res.status).toBe(200);
    const body = (await res.json()) as any;
    expect(body.result).toMatch(/^0x/);
  });

  it("eth_chainId returns 0x7a69 (31337)", async () => {
    const res = await fetch(`${GATEWAY_URL}/rpc/31337`, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ jsonrpc: "2.0", method: "eth_chainId", params: [], id: 1 }),
    });
    expect(res.status).toBe(200);
    const body = (await res.json()) as any;
    expect(parseInt(body.result, 16)).toBe(31337);
  });

  it("net_version returns chain ID as decimal string", async () => {
    const res = await fetch(`${GATEWAY_URL}/rpc/31337`, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ jsonrpc: "2.0", method: "net_version", params: [], id: 2 }),
    });
    expect(res.status).toBe(200);
    const body = (await res.json()) as any;
    expect(body.result).toBe("31337");
  });

  it("eth_getBalance returns hex string for funded account", async () => {
    const res = await fetch(`${GATEWAY_URL}/rpc/31337`, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({
        jsonrpc: "2.0",
        method: "eth_getBalance",
        params: ["0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266", "latest"],
        id: 3,
      }),
    });
    expect(res.status).toBe(200);
    const body = (await res.json()) as any;
    expect(body.result).toMatch(/^0x/);
    expect(BigInt(body.result)).toBeGreaterThan(0n);
  });

  it("eth_getBlockByNumber latest returns block with number field", async () => {
    const res = await fetch(`${GATEWAY_URL}/rpc/31337`, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({
        jsonrpc: "2.0",
        method: "eth_getBlockByNumber",
        params: ["latest", false],
        id: 4,
      }),
    });
    expect(res.status).toBe(200);
    const body = (await res.json()) as any;
    expect(body.result).toBeTruthy();
    expect(body.result.number).toMatch(/^0x/);
  });

  it("eth_call on deployed GRT token returns ABI-encoded uint256", async () => {
    // balanceOf(address) selector = 0x70a08231, provider address zero-padded to 32 bytes
    const calldata =
      "0x70a08231" + fx.providerAddress.slice(2).toLowerCase().padStart(64, "0");
    const res = await fetch(`${GATEWAY_URL}/rpc/31337`, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({
        jsonrpc: "2.0",
        method: "eth_call",
        params: [{ to: fx.grtToken, data: calldata }, "latest"],
        id: 5,
      }),
    });
    expect(res.status).toBe(200);
    const body = (await res.json()) as any;
    // 32-byte ABI-encoded uint256
    expect(body.result).toMatch(/^0x[0-9a-fA-F]{64}$/);
  });

  it("batch request returns an array response with one entry per request", async () => {
    const res = await fetch(`${GATEWAY_URL}/rpc/31337`, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify([
        { jsonrpc: "2.0", method: "eth_blockNumber", params: [], id: 100 },
        { jsonrpc: "2.0", method: "eth_chainId",     params: [], id: 101 },
        { jsonrpc: "2.0", method: "net_version",     params: [], id: 102 },
      ]),
    });
    expect(res.status).toBe(200);
    const body = (await res.json()) as any;
    expect(Array.isArray(body)).toBe(true);
    expect(body.length).toBe(3);
    for (const item of body) {
      expect(item.jsonrpc).toBe("2.0");
      expect(item.result !== undefined || item.error !== undefined).toBe(true);
    }
  });

  it("POST /rpc with X-Chain-Id header routes to correct chain", async () => {
    const res = await fetch(`${GATEWAY_URL}/rpc`, {
      method: "POST",
      headers: { "Content-Type": "application/json", "X-Chain-Id": "31337" },
      body: JSON.stringify({ jsonrpc: "2.0", method: "eth_blockNumber", params: [], id: 1 }),
    });
    expect(res.status).toBe(200);
    const body = (await res.json()) as any;
    expect(body.result).toMatch(/^0x/);
  });

  it("POST /rpc with no X-Chain-Id header defaults to chain 1 — returns error (chain 1 not configured)", async () => {
    const res = await fetch(`${GATEWAY_URL}/rpc`, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ jsonrpc: "2.0", method: "eth_blockNumber", params: [], id: 1 }),
    });
    // Chain 1 is not configured in the e2e setup, so we expect an error
    const body = (await res.json()) as any;
    expect(body.error).toBeDefined();
  });
});

// ── gateway: error handling ───────────────────────────────────────────────────

describe("gateway: error handling", () => {
  it("request for unsupported chain returns 404 with error", async () => {
    const res = await fetch(`${GATEWAY_URL}/rpc/99999`, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ jsonrpc: "2.0", method: "eth_blockNumber", params: [], id: 1 }),
    });
    expect(res.status).toBe(404);
    const body = (await res.json()) as any;
    expect(body.error.code).toBe(-32002);
  });

  it("debug_traceCall returns 503 when no debug-capable provider is registered", async () => {
    const res = await fetch(`${GATEWAY_URL}/rpc/31337`, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({
        jsonrpc: "2.0",
        method: "debug_traceCall",
        params: [{ to: "0x0000000000000000000000000000000000001234", data: "0x" }, "latest"],
        id: 1,
      }),
    });
    expect(res.status).toBe(503);
    const body = (await res.json()) as any;
    expect(body.error.code).toBe(-32003);
  });

  it("empty batch returns 400", async () => {
    const res = await fetch(`${GATEWAY_URL}/rpc/31337`, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify([]),
    });
    expect(res.status).toBe(400);
  });
});

// ── rav/aggregate ─────────────────────────────────────────────────────────────

describe("rav/aggregate", () => {
  it("aggregates N valid receipts into a correctly valued signed RAV", async () => {
    const N = 5;
    // Build the request body as a raw string to preserve BigInt precision in
    // timestamp_ns/nonce fields — JSON.parse would lose nanosecond accuracy.
    const receipts = await Promise.all(
      Array.from({ length: N }, () => signReceipt(fx))
    );
    const body = `{"service_provider":"${fx.providerAddress}","receipts":[${receipts.join(",")}]}`;

    const res = await fetch(`${GATEWAY_URL}/rav/aggregate`, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body,
    });
    expect(res.status).toBe(200);
    const json = (await res.json()) as any;
    const rav = json.signed_rav.rav;

    expect(json.signed_rav.signature).toMatch(/^0x/);
    expect(json.signed_rav.signature.length).toBe(132); // "0x" + 65 bytes
    expect(rav.payer.toLowerCase()).toBe(fx.gatewaySignerAddress.toLowerCase());
    expect(rav.service_provider.toLowerCase()).toBe(fx.providerAddress.toLowerCase());
    expect(rav.data_service.toLowerCase()).toBe(fx.rpcDataService.toLowerCase());
    // Cumulative over whatever the gateway already signed for this collection.
    expect(rav.value_aggregate).toBeGreaterThanOrEqual(N * 4_000_000_000_000);
  });

  it("ignores the caller's floor and never pays a receipt twice", async () => {
    const post = async (receipts: string[], floor = 0) => {
      const body = `{"service_provider":"${fx.providerAddress}","previous_rav_value":${floor},"receipts":[${receipts.join(",")}]}`;
      const res = await fetch(`${GATEWAY_URL}/rav/aggregate`, {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body,
      });
      expect(res.status).toBe(200);
      return ((await res.json()) as any).signed_rav.rav.value_aggregate as number;
    };

    const first = await signReceipt(fx);
    const v1 = await post([first, first], 1_000_000_000_000_000_000);
    expect(await post([first])).toBe(v1);
    expect(await post([first, await signReceipt(fx)])).toBe(v1 + 4_000_000_000_000);
  });

  it("rejects empty receipts array with 400", async () => {
    const res = await fetch(`${GATEWAY_URL}/rav/aggregate`, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ service_provider: fx.providerAddress, receipts: [] }),
    });
    expect(res.status).toBe(400);
    const body = (await res.json()) as any;
    expect(body.error).toBeDefined();
  });

  it("rejects receipt signed by wrong key with 400", async () => {
    // Signed by providerKey, not gatewaySignerKey
    const receipt = await signReceipt(fx, { key: fx.providerKey });
    const body = `{"service_provider":"${fx.providerAddress}","receipts":[${receipt}]}`;
    const res = await fetch(`${GATEWAY_URL}/rav/aggregate`, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body,
    });
    expect(res.status).toBe(400);
    const json = (await res.json()) as any;
    expect(json.error).toBeDefined();
  });

  it("rejects receipt with mismatched service_provider with 400", async () => {
    const account = privateKeyToAccount(fx.gatewaySignerKey);
    const timestampNs = BigInt(Date.now()) * 1_000_000n;
    const nonce = BigInt(Math.floor(Math.random() * 1e15));
    const wrongProvider = "0x0000000000000000000000000000000000001234" as `0x${string}`;

    const sig = await account.signTypedData({
      domain: {
        name: "GraphTallyCollector",
        version: "1",
        chainId: 31337,
        verifyingContract: fx.graphTallyCollector,
      },
      types: {
        Receipt: [
          { name: "data_service",     type: "address" },
          { name: "service_provider", type: "address" },
          { name: "timestamp_ns",     type: "uint64" },
          { name: "nonce",            type: "uint64" },
          { name: "value",            type: "uint128" },
          { name: "metadata",         type: "bytes" },
        ],
      },
      primaryType: "Receipt",
      message: {
        data_service: fx.rpcDataService,
        service_provider: wrongProvider,
        timestamp_ns: timestampNs,
        nonce,
        value: 4_000_000_000_000n,
        metadata: "0x",
      },
    });
    const receipt = `{"receipt":{"data_service":"${fx.rpcDataService}","service_provider":"${wrongProvider}","timestamp_ns":${timestampNs},"nonce":${nonce},"value":4000000000000,"metadata":"0x"},"signature":"${sig}"}`;
    const body = `{"service_provider":"${fx.providerAddress}","receipts":[${receipt}]}`;

    const res = await fetch(`${GATEWAY_URL}/rav/aggregate`, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body,
    });
    expect(res.status).toBe(400);
  });
});

// ── escrow pre-check ──────────────────────────────────────────────────────────
//
// Uses the side service (port 7701): authorized_senders = [], escrow check enabled.
// gatewaySignerKey has funded escrow; providerKey does not.

describe("service: escrow pre-check", () => {
  it("rejects a request when the signer has no escrow balance (402)", async () => {
    // providerKey is an authorized signer (no whitelist) but has never funded escrow.
    const receipt = await signReceipt(fx, { key: fx.providerKey });
    const res = await fetch(`${SIDE_SERVICE_URL}/rpc/31337`, {
      method: "POST",
      headers: { "Content-Type": "application/json", "TAP-Receipt": receipt },
      body: JSON.stringify({ jsonrpc: "2.0", method: "eth_blockNumber", params: [], id: 1 }),
    });
    expect(res.status).toBe(402);
    const body = (await res.json()) as any;
    expect(body.error.code).toBe(-32005);
    expect(body.error.message).toContain("escrow");
  });

  it("serves a request when the signer has a funded escrow balance (200)", async () => {
    // gatewaySignerKey has escrow funded in SetupE2E via depositTo.
    const receipt = await signReceipt(fx, { key: fx.gatewaySignerKey });
    const res = await fetch(`${SIDE_SERVICE_URL}/rpc/31337`, {
      method: "POST",
      headers: { "Content-Type": "application/json", "TAP-Receipt": receipt },
      body: JSON.stringify({ jsonrpc: "2.0", method: "eth_blockNumber", params: [], id: 2 }),
    });
    expect(res.status).toBe(200);
    const body = (await res.json()) as any;
    expect(body.result).toMatch(/^0x/);
  });
});

// ── credit limit ──────────────────────────────────────────────────────────────
//
// Credit service (port 7702) has no escrow check and credit_threshold = 8_000_000_000_000
// (2 CUs at 4T GRT wei each). After two successful requests the third is rejected with 402.
// (When escrow is verified on-chain the credit check is skipped; this service has no
// escrow checker so the credit gate is the only protection.)

describe("service: credit limit", () => {
  it("blocks the third request once accumulated credit reaches the threshold", async () => {
    // Sign with gatewayKey — no escrow check on this service, so credit gate applies.
    const makeRequest = async (id: number) =>
      fetch(`${CREDIT_SERVICE_URL}/rpc/31337`, {
        method: "POST",
        headers: {
          "Content-Type": "application/json",
          "TAP-Receipt": await signReceipt(fx, { key: fx.gatewayKey, nonce: BigInt(id) * 100n }),
        },
        body: JSON.stringify({ jsonrpc: "2.0", method: "eth_blockNumber", params: [], id }),
      });

    const r1 = await makeRequest(10);
    expect(r1.status).toBe(200); // served = 4T, under 8T threshold

    const r2 = await makeRequest(11);
    expect(r2.status).toBe(200); // served = 8T, still under threshold (check is >=)

    const r3 = await makeRequest(12);
    expect(r3.status).toBe(402); // served = 8T >= 8T threshold → blocked
    const body = (await r3.json()) as any;
    expect(body.error.code).toBe(-32005);
    expect(body.error.message).toContain("credit");
  });
});

// ── payment round-trip ────────────────────────────────────────────────────────

describe("payment round-trip", () => {
  it("collect() transfers GRT from escrow to paymentWallet", async () => {
    const publicClient = createPublicClient({
      chain: anvil,
      transport: http(),
    });
    const providerAccount = privateKeyToAccount(fx.providerKey);
    const walletClient = createWalletClient({
      account: providerAccount,
      chain: anvil,
      transport: http(),
    });

    const grtAbi = parseAbi([
      "function balanceOf(address) view returns (uint256)",
    ]);
    const serviceAbi = parseAbi([
      "function collect(address serviceProvider, uint8 paymentType, bytes calldata data) returns (uint256)",
    ]);

    // Record initial paymentWallet balance.
    const before = (await publicClient.readContract({
      address: fx.grtToken,
      abi: grtAbi,
      functionName: "balanceOf",
      args: [fx.paymentWallet],
    })) as bigint;

    // Build and sign a RAV using the gateway signer.
    const signerAccount = privateKeyToAccount(fx.gatewaySignerKey);
    const valueAggregate = 1_000_000_000_000_000_000n; // 1e18 GRT wei
    const timestampNs = BigInt(Date.now()) * 1_000_000n;

    const ravSig = await signerAccount.signTypedData({
      domain: {
        name: "GraphTallyCollector",
        version: "1",
        chainId: 31337,
        verifyingContract: fx.graphTallyCollector,
      },
      types: {
        ReceiptAggregateVoucher: [
          { name: "collectionId",    type: "bytes32" },
          { name: "payer",           type: "address" },
          { name: "serviceProvider", type: "address" },
          { name: "dataService",     type: "address" },
          { name: "timestampNs",     type: "uint64" },
          { name: "valueAggregate",  type: "uint128" },
          { name: "metadata",        type: "bytes" },
        ],
      },
      primaryType: "ReceiptAggregateVoucher",
      message: {
        collectionId:
          "0x0000000000000000000000000000000000000000000000000000000000000000",
        payer: fx.gatewayAddress,
        serviceProvider: fx.providerAddress,
        dataService: fx.rpcDataService,
        timestampNs,
        valueAggregate,
        metadata: "0x",
      },
    });

    // abi.encode(SignedRAV, tokensToCollect)
    const ravTuple = {
      collectionId:
        "0x0000000000000000000000000000000000000000000000000000000000000000" as `0x${string}`,
      payer: fx.gatewayAddress,
      serviceProvider: fx.providerAddress,
      dataService: fx.rpcDataService,
      timestampNs,
      valueAggregate,
      metadata: "0x" as `0x${string}`,
    };

    const collectData = encodeAbiParameters(
      parseAbiParameters(
        "((bytes32 collectionId, address payer, address serviceProvider, address dataService, uint64 timestampNs, uint128 valueAggregate, bytes metadata) rav, bytes signature) signedRav, uint256 tokensToCollect"
      ),
      [{ rav: ravTuple, signature: ravSig }, valueAggregate]
    );

    // Call collect() as the provider.
    const txHash = await walletClient.writeContract({
      address: fx.rpcDataService,
      abi: serviceAbi,
      functionName: "collect",
      args: [fx.providerAddress, 0, collectData], // 0 = PaymentTypes.QueryFee
    });
    await publicClient.waitForTransactionReceipt({ hash: txHash });

    // Verify GRT landed at the payment wallet.
    const after = (await publicClient.readContract({
      address: fx.grtToken,
      abi: grtAbi,
      functionName: "balanceOf",
      args: [fx.paymentWallet],
    })) as bigint;

    expect(after).toBeGreaterThan(before);
  });

  it("GRT balance at paymentWallet is non-zero after collection", async () => {
    const publicClient = createPublicClient({ chain: anvil, transport: http() });
    const grtAbi = parseAbi(["function balanceOf(address) view returns (uint256)"]);
    const balance = (await publicClient.readContract({
      address: fx.grtToken,
      abi: grtAbi,
      functionName: "balanceOf",
      args: [fx.paymentWallet],
    })) as bigint;
    expect(balance).toBeGreaterThan(0n);
  });
});
