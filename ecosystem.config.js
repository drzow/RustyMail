module.exports = {
  apps : [{
    name: 'rustymail-backend',
    script: './target/release/rustymail-server',
  }, {
    name: 'rustymail-frontend',
    script: 'npm',
    args: 'run dev',
    cwd: './frontend/rustymail-app-main',
  }]
};
