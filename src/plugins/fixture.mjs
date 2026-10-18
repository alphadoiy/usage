// Synthetic provider contract fixture. No vendor credentials or external network.
let refreshAttempts = 0;
const make =
  (id) =>
  async ({ client }) => ({
    config: async (c) => {
      c.provider[id] = {
        name: id,
        npm: "@ai-sdk/openai-compatible",
        api: "http://127.0.0.1:1/v1",
        models: {
          "fixture-model": {
            name: "Fixture",
            limit: { context: 8192, output: 1024 },
          },
        },
      };
    },
    provider: {
      id,
      models: async (p, { auth }) => ({
        ...p.models,
        [`account-${auth.uid}`]: { name: auth.uid },
      }),
    },
    "chat.headers": async (i, o) => {
      o.headers["x-fixture-session"] = i.sessionID;
    },
    auth: {
      provider: id,
      refreshLead: 300000,
      refresh: async (a) => {
        refreshAttempts++;
        await new Promise((ok) => setTimeout(ok, 30));
        return { access: `fresh-${a.uid}`, expires: Date.now() + 3600000 };
      },
      methods: [
        {
          type: "oauth",
          label: "Synthetic browser",
          authorize: async (inputs) => ({
            url: "https://example.test/authorize",
            instructions: "fixture",
            method: "auto",
            callback: async () => {
              await new Promise((ok) =>
                setTimeout(ok, Number(inputs?.delay ?? 0)),
              );
              return {
                type: "success",
                access: `fresh-${inputs?.uid ?? "user"}`,
                refresh: "synthetic-refresh",
                expires: Date.now() + 3600000,
                uid: inputs?.uid ?? "user",
                accountId: inputs?.uid ?? "user",
              };
            },
          }),
        },
        {
          type: "api",
          label: "Synthetic API",
          authorize: async (inputs) => ({ type: "success", key: inputs.key }),
        },
        { type: "api", label: "Raw API key" },
      ],
      loader: async (getAuth) => ({
        baseURL: "http://127.0.0.1:1/v1",
        fetch: async (url, init) => {
          const a = await getAuth();
          const headers = new Headers(init.headers);
          const body = JSON.parse(init.body);
          if (a.access !== `fresh-${a.uid}`)
            throw new Error("fixture token was not refreshed");
          const t1 = {
            id: "fixture-chat",
            model: body.model,
            choices: [
              {
                index: 0,
                delta: { reasoning_content: "Reason." },
                finish_reason: null,
              },
            ],
          };
          const t2 = {
            id: "fixture-chat",
            model: body.model,
            choices: [
              {
                index: 0,
                delta: {
                  content: `${a.uid}:${headers.get("x-fixture-session")}`,
                },
                finish_reason: null,
              },
            ],
          };
          const t3 = {
            choices: [
              {
                index: 0,
                delta: {
                  tool_calls: [
                    {
                      index: 0,
                      id: "fixture-call",
                      function: { name: "fixture_tool", arguments: '{"x":1}' },
                    },
                  ],
                },
                finish_reason: "tool_calls",
              },
            ],
            usage: {
              prompt_tokens: 50,
              completion_tokens: 12,
              total_tokens: 62,
            },
          };
          return new Response(
            [t1, t2, t3].map((v) => `data: ${JSON.stringify(v)}\n\n`).join("") +
              "data: [DONE]\n\n",
            { headers: { "content-type": "text/event-stream" } },
          );
        },
      }),
      usage: async (getAuth) => {
        const a = await getAuth();
        if (a.uid === "slow") await new Promise((ok) => setTimeout(ok, 500));
        return {
          user: a.uid,
          refresh_attempts: refreshAttempts,
          plan: "Fixture",
          windows: [
            {
              name: "Credits",
              used: 25,
              amount: 25,
              limit: 100,
              unit: "credits",
            },
          ],
        };
      },
    },
  });
export const FixturePlugin = make("fixture");
export const OtherPlugin = make("fixture-ai");
export default FixturePlugin;
