export type ClosableReadHandle = {
  close(): void | Promise<void>;
};

export type WorkspaceReadSessionCacheOptions<T extends ClosableReadHandle> = {
  open: () => T | Promise<T>;
  idleTtlMs?: number;
  serializeOperations?: boolean;
};

export class WorkspaceReadSessionCache<T extends ClosableReadHandle> {
  private handle?: T;
  private openPromise?: Promise<T>;
  private activeReaders = 0;
  private lastReadAt = 0;
  private idleTimer?: ReturnType<typeof setTimeout>;
  private operationTail: Promise<void> = Promise.resolve();
  private closePromise?: Promise<void>;
  private closeResolve?: () => void;
  private closed = false;

  constructor(private readonly options: WorkspaceReadSessionCacheOptions<T>) {}

  async withRead<R>(operation: (handle: T) => Promise<R>): Promise<R> {
    if (this.closed) {
      throw new Error("Workspace read session cache is closed.");
    }
    if (this.idleTimer) {
      clearTimeout(this.idleTimer);
      this.idleTimer = undefined;
    }

    this.activeReaders += 1;
    try {
      const handle = await this.getOrOpen();
      if (this.options.serializeOperations === false) {
        return await operation(handle);
      }
      return await this.runSerial(() => operation(handle));
    } finally {
      this.activeReaders -= 1;
      this.lastReadAt = Date.now();
      if (this.activeReaders === 0) {
        this.closeResolve?.();
        this.scheduleIdleClose();
      }
    }
  }

  async close(): Promise<void> {
    if (this.closed && !this.handle && !this.openPromise) {
      return;
    }
    this.closed = true;
    if (this.idleTimer) {
      clearTimeout(this.idleTimer);
      this.idleTimer = undefined;
    }
    if (this.activeReaders > 0) {
      this.closePromise ??= new Promise<void>((resolve) => {
        this.closeResolve = resolve;
      });
      await this.closePromise;
    }
    if (this.openPromise) {
      await this.openPromise;
    }
    await this.closeHandle();
  }

  snapshot(): { open: boolean; activeReaders: number } {
    return {
      open: this.handle !== undefined || this.openPromise !== undefined,
      activeReaders: this.activeReaders,
    };
  }

  private async getOrOpen(): Promise<T> {
    if (this.handle) {
      return this.handle;
    }
    this.openPromise ??= Promise.resolve(this.options.open());
    try {
      this.handle = await this.openPromise;
      return this.handle;
    } finally {
      this.openPromise = undefined;
    }
  }

  private async runSerial<R>(operation: () => Promise<R>): Promise<R> {
    const previous = this.operationTail;
    let release!: () => void;
    this.operationTail = new Promise<void>((resolve) => {
      release = resolve;
    });
    await previous;
    try {
      return await operation();
    } finally {
      release();
    }
  }

  private scheduleIdleClose(): void {
    const idleTtlMs = this.options.idleTtlMs ?? 60_000;
    const lastReadAt = this.lastReadAt;
    if (idleTtlMs === 0) {
      void this.closeHandle();
      return;
    }
    this.idleTimer = setTimeout(() => {
      if (this.activeReaders === 0 && this.lastReadAt === lastReadAt) {
        void this.closeHandle();
      }
    }, idleTtlMs);
    this.idleTimer.unref?.();
  }

  private async closeHandle(): Promise<void> {
    const handle = this.handle;
    this.handle = undefined;
    if (handle) {
      await handle.close();
    }
  }
}
