Quality gate "e2e" failed (exit n/a):

```

Run 'docker run --help' for more information
"}], "mocks": [{"name": "aimock", "port": 4010, "status": "running", "routeCount": 0}], "totalDuration": 3257, "preflight": {"overall": "healthy", "checks": [{"name": "docker_daemon", "status": "pass", "message": "Docker daemon is reachable", "details": {}, "duration": 433}, {"name": "disk_space", "status": "pass", "message": "60GB available (threshold: 2GB)", "details": {"availableGB": 60, "requiredGB": 2, "threshold": "2GB"}, "duration": 140}, {"name": "orphan_resources", "status": "pass", "message": "No orphaned resources detected", "details": {"orphans": [], "count": 0}, "duration": 276}], "timestamp": 1783660040207, "duration": 850}, "orphanCleanup": {"found": [], "removed": [], "failed": [], "duration": 0}}, "timestamp": 1783660042614}
[e2e-gate] smoke FAILED ✗
{"success": false, "error": {"code": "NOT_RUNNING", "message": "Environment not set up. Call argus_setup first."}, "timestamp": 1783660043154}

```


## Evaluator 输出失败

Evaluator 跑结构化输出失败（3 次重试仍非合法 JSON）。这通常是 evaluator 模型自身的输出格式问题，不一定是代码错。
请 generator 重新自评一遍 contract 各验收点，确认实现无误。