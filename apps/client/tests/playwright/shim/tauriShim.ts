type Envelope = { event: string; id: number; payload: unknown };
type Handler = (e: Envelope) => void;
type Unlisten = () => void;

const listeners: Map<string, Set<Handler>> = new Map();
const queues: Map<string, Envelope[]> = new Map();
let counter = 1;

export function listen(event: string, handler: Handler): Promise<Unlisten> {
    let set = listeners.get(event);
    if (!set) {
        set = new Set();
        listeners.set(event, set);
    }
    set.add(handler);
    const q = queues.get(event);
    if (q && q.length > 0) {
        for (const env of q.splice(0, q.length)) {
            handler(env);
        }
    }
    counter++;
    return Promise.resolve(() => {
        set!.delete(handler);
    });
}

export function __emit(event: string, payload: unknown): number {
    const env: Envelope = { event, id: -1, payload };
    const set = listeners.get(event);
    if (!set || set.size === 0) {
        let q = queues.get(event);
        if (!q) {
            q = [];
            queues.set(event, q);
        }
        q.push(env);
        return 0;
    }
    for (const h of set) {
        h(env);
    }
    return set.size;
}

export function __reset(): void {
    listeners.clear();
    queues.clear();
    counter = 1;
}
