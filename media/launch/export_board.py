"""Export real Pika renderer output with wholly synthetic inventory. No DB/SSH."""
import json
from pathlib import Path

from pikamux.models import Session, Status, FleetSession, FleetNode, ExpertProfile
from pikamux.monitor import MonitorState, render_monitor
from pikamux.experts import ExpertCardState
from pikamux.explain import explain_session
from pikamux.status_projection import StatusObservation

NOW = 1788829200.0


def build():
    rows = [
        Session('codex', '11111111-1111-4111-8111-111111111111', name='release-notes',
                cwd='/demo/release', status=Status.NEEDS_YOU.value, live=True,
                home_state='exact-live', tmux_pane='%11', unread=True,
                attention_reason='permission', last_event_at=NOW-42, last_activity_at=NOW-42),
        Session('claude', '22222222-2222-4222-8222-222222222222', name='design-system',
                cwd='/demo/design', status=Status.WORKING.value, live=True,
                home_state='exact-live', tmux_pane='%12', last_activity_at=NOW-8),
        Session('opencode', '33333333-3333-4333-8333-333333333333', name='test-suite',
                cwd='/demo/tests', status=Status.WORKING.value, live=True,
                home_state='exact-live', tmux_pane='%13', last_activity_at=NOW-3),
        Session('codex', '44444444-4444-4444-8444-444444444444', name='api-migration',
                cwd='/demo/api', status=Status.READY.value, unread=True,
                live=True, home_state='exact-live', tmux_pane='%14',
                attention_reason='completed', last_event_at=NOW-93, last_activity_at=NOW-93),
    ]
    remote = FleetSession('demo-remote', 'buildbox', Session('codex',
        '55555555-5555-4555-8555-555555555555', name='architecture',
        cwd='/demo/architecture', status=Status.WORKING.value, live=True,
        home_state='exact-live', tmux_pane='%21', last_activity_at=NOW-11), seen_at=NOW)
    rows.append(remote)
    state = MonitorState(sessions=rows, last_update=NOW,
        selected_key=rows[0].key, preview_key=rows[0].key,
        preview_lines=['Approval requested before running the release step.'],
        preview_updated_at=NOW,
        machines=[FleetNode('demo-remote', 'buildbox', 'demo-host', status='ready', last_seen=NOW)])
    frames = {}
    for name, selected in [('overview', rows[0]), ('result', rows[3]), ('remote', remote)]:
        base = selected.session if isinstance(selected, FleetSession) else selected
        base.transcript_path = f'/demo/{base.name}/conversation.jsonl'
        base.branch = 'main'
        profile = ExpertProfile(base.provider, base.session_id,
            'Release safety and changelog ownership.' if name == 'overview' else
            'API migration and compatibility.' if name == 'result' else
            'System architecture and design decisions.',
            ('release checks', 'compatibility') if name != 'remote' else ('identity', 'recovery', 'consultations'),
            updated_at=NOW-60, current_state='Waiting for release approval.' if name == 'overview' else
            'Migration result ready for review.' if name == 'result' else 'Reviewing exact-thread recovery.',
            current_state_updated_at=NOW-30)
        state.expert_cards[selected.key] = ExpertCardState(selected, profile, 'CURRENT', 'synthetic fixture')
        state.expert_freshness[selected.key] = {'current_state_status': 'CURRENT'}
        state.expert_cards_updated_at = NOW
        if not isinstance(selected, FleetSession):
            state.explanations[selected.key] = explain_session(selected, [StatusObservation(
                'lifecycle', base.status, base.unread, base.attention_reason, None,
                base.last_event_at, 'provider hook')], now=NOW)
        state.selected_key = selected.key
        state.preview_key = selected.key
        state.preview_lines = ['Awaiting your approval.'] if name == 'overview' else ['Sample project output.']
        frames[name] = render_monitor(state, width=112, height=25, now=NOW, color=True).ansi
    return {'frames': frames, 'provenance': 'Real Pika renderer; synthetic inventory; not a live recording'}


if __name__ == '__main__':
    path = Path(__file__).with_name('board.js')
    path.write_text('window.PIKA_BOARD = ' + json.dumps(build(), ensure_ascii=False) + ';\n')
    print(path)
