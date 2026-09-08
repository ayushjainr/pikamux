// User-approved fictional story. Dialogue, cards, statuses and interface are
// illustrative, not quotations or evidence from the earlier live attempts.
window.PIKA_CROSS_PROJECT = {
  task: 'Add a weekly activity view to the dashboard.',
  expertName: 'Weekly report exporter',
  scopeExcerpt: 'Built the scheduled report export, including the Activity service connection and its access checks.',
  topics: ['Service authentication', 'Weekly activity exports'],
  searchCommand: 'pika experts "Activity service" --json',
  question: 'I’m connecting the dashboard to the Activity service. I get a token, but data requests return 403. Did you need anything beyond the documented setup?',
  answer: 'Yes. The data gateway still uses the old reports-v1 audience. Request a separate token for that; keep sign-in unchanged. We isolated it in auth_client.py and added a regression test.',
  artifacts: ['auth_client.py', 'test_data_access.py'],
  weeks: [{label:'Week 34',events:84},{label:'Week 35',events:112},{label:'Week 36',events:97}]
};
