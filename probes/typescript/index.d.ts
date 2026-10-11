// types for ccc's node probe - the calls `ccc probe typescript` writes into a copy of the project

// one test, named the way its runner selects it
export interface Fixture {
  name: string;
  file: string;
  language: string;
  kind: string;
}

// counts one run of a function for the fixture this work runs for
export function hit(file: string, name: string, line: number): void;
// a test's callback, run as a fixture until it returns, resolves or calls done
export function fixture<F>(name: string, file: string, kind: string, fn: F): F;
// runs fn as work for f
export function bind<T>(f: Fixture | undefined, fn: () => T): T;
export function current(): Fixture | undefined;
// the fixture as w3c baggage
export function baggage(f: Fixture): string;
export function fromBaggage(header: string | string[] | undefined): Fixture | undefined;
// writes what fixtures ran in this process that no test of its own ended
export function flush(): void;
