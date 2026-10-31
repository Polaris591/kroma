import type { Bootstrap, SessionSnapshot } from './api';

/** Apply the one durable snapshot emitted at a task boundary. */
export function applySnapshot(data: Bootstrap, snapshot: SessionSnapshot): Bootstrap {
  const known = data.sessions.some(session => session.id === snapshot.session_id);
  const sessions = known
    ? data.sessions.map(session => session.id === snapshot.session_id
      ? {...session, recent: true} : session)
    : snapshot.session_id && snapshot.session_id !== 'unknown'
      ? [{id: snapshot.session_id, name: null, recent: true}, ...data.sessions]
      : data.sessions;
  return {...data, snapshot, sessions};
}

/** Ignore late events from a task that no longer owns the visible session. */
export function isActiveTask(generation: number, current: number): boolean {
  return generation === current;
}
