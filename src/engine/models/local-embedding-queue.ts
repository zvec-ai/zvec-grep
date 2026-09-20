type PendingOperation = {
  exclusive: boolean;
  start: () => void;
  reject: (cause: unknown) => void;
};

/** A FIFO inference queue with barriers for runtime replacement and disposal. */
export class LocalEmbeddingQueue {
  private readonly pending: PendingOperation[] = [];
  private limit: number | undefined;
  private resolving = false;
  private active = 0;
  private exclusive = false;

  constructor(private readonly resolveLimit: () => Promise<number>) {}

  run<T>(operation: () => Promise<T>, exclusive = false): Promise<T> {
    return new Promise<T>((resolve, reject) => {
      this.pending.push({
        exclusive,
        reject,
        start: () => {
          this.active++;
          this.exclusive = exclusive;
          void Promise.resolve()
            .then(operation)
            .then(resolve, reject)
            .finally(() => {
              this.active--;
              if (exclusive) this.exclusive = false;
              this.pump();
            });
        },
      });
      this.pump();
    });
  }

  private pump(): void {
    if (this.pending.length === 0 || this.exclusive) return;
    if (this.limit === undefined) {
      if (this.resolving) return;
      this.resolving = true;
      void Promise.resolve()
        .then(this.resolveLimit)
        .then(
          (limit) => {
            this.limit = limit;
            this.resolving = false;
            this.pump();
          },
          (cause: unknown) => {
            this.resolving = false;
            for (const operation of this.pending.splice(0)) {
              operation.reject(cause);
            }
          },
        );
      return;
    }

    while (this.pending.length > 0 && this.active < this.limit) {
      if (this.pending[0].exclusive && this.active > 0) return;
      const operation = this.pending.shift()!;
      operation.start();
      if (operation.exclusive) return;
    }
  }
}
