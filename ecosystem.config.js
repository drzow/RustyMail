module.exports = {
  apps : [{
    name: 'rustymail-backend',
    script: './target/release/rustymail-server',
    // jemalloc (tikv-jemalloc-sys, prefixed -> _RJEM_ env): run a background
    // thread that purges decayed pages back to the OS, so RSS does not stay
    // pinned at the high-water mark after a transient spike + idle.
    env: {
      _RJEM_MALLOC_CONF: 'background_thread:true,dirty_decay_ms:5000,muzzy_decay_ms:5000'
    },
  }, {
    name: 'rustymail-frontend',
    script: 'npm',
    args: 'run dev',
    cwd: './frontend/rustymail-app-main',
  }]
};
