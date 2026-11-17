// Minimal, offline implementation of the official Gemini Core API used to
// exercise the Rust ACP adapter without credentials or network access.
export const DEFAULT_GEMINI_MODEL = 'gemini-2.5-pro';
export const VALID_GEMINI_MODELS = ['gemini-2.5-pro', 'gemini-2.5-flash'];
export const AuthType = { USE_GEMINI: 'gemini', USE_VERTEX_AI: 'vertex' };
export const GeminiEventType = { Content: 'content', Finished: 'finished', ToolCallRequest: 'tool_call_request' };
export const MessageBusType = { TOOL_CALLS_UPDATE: 'tool-calls-update' };

export class Config {
  constructor(options) { this.options = options; this.model = options.model; this.listener = undefined; }
  async initialize() {}
  async refreshAuth(auth) {
    if (process.env.NORI_GEMINI_EXPECT_AUTH !== auth) throw new Error(`unexpected auth mode: ${auth}`);
  }
  getGeminiClient() {
    return {
      initialize: async () => {},
      sendMessageStream: async function* (parts, signal) {
        if (parts.some((part) => part.text === 'cancel-case')) {
          yield { type: GeminiEventType.Content, value: 'cancel fixture started' };
          if (!signal.aborted) await new Promise((resolve) => signal.addEventListener('abort', resolve, {once:true}));
          return;
        }
        if (parts.some((part) => part.text === 'tool-case')) {
          yield { type: GeminiEventType.ToolCallRequest, value: {callId:'call-1', name:'fixture', args:{query:'x'}} };
          return;
        }
        yield { type: GeminiEventType.Content, value: 'offline fake Core response' };
        yield { type: GeminiEventType.Finished, value: { reason: 'STOP' } };
      },
    };
  }
  getMessageBus() { return { subscribe: (_kind, listener) => { this.listener = listener; } }; }
  getModel() { return this.model; }
  setModel(model) { this.model = model; }
  getToolRegistry() { return {}; }
}

export async function scheduleAgentTools(config, calls) {
  const response = {
    resultDisplay: 'human display',
    responseParts: [{text:'model-visible text'}, {inlineData:{mimeType:'image/png',data:'aGVsbG8='}}],
    data: {content:[{type:'text',text:'raw text'}], structuredContent:{count:7}, isError:false},
  };
  config.listener({toolCalls:[{status:'success', request:calls[0], response}]});
  return [{response}];
}
