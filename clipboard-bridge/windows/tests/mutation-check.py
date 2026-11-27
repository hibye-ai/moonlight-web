#!/usr/bin/env python3
"""Mutation controls for the AR-08 installer transaction tests.

Each mutation reintroduces one defect the finding is about: a rollback layer
that is not restored, a backup that is not taken, a postcheck that does not
fail, a previous helper that is not brought back.The test suite is only
meaningful if it turns red for every one of these, so each mutation names the
scenario that must detect it.

Usage: python3 mutation-check.py <windows-source-dir> <pwsh-binary> <work-dir>
Exit code 0 only when the unmutated baseline passes and every mutation is
detected by its named scenario.
"""
import json
import shutil
import subprocess
import sys
from pathlib import Path

INSTALL = "install.ps1"
TESTS = Path("tests") / "install.Tests.ps1"

# (name, old, new, scenario that must fail, what the defect is)
MUTATIONS = [
    (
        "rollback-leaves-the-new-firewall-rule",
        " try{\n  Invoke-Op $ops 'RemoveFirewall' @{Name=$FirewallName} $context\n  if($previous.Firewall){",
        " try{\n  if($previous.Firewall){",
        "fresh-install-failure-removes-task-firewall-and-files",
        "rollback never removes the rule this run created",
    ),
    (
        "rollback-re-registers-a-default-task-instead-of-the-previous-xml",
        "   Invoke-Op $ops 'RegisterTask' @{Name=$TaskName;Xml=$previous.Task.Xml} $context",
        "   Invoke-Op $ops 'RegisterTask' @{Name=$TaskName;Definition=@{Action=@(@{Execute=$executable});Settings=@{RestartCount=999;RestartInterval='PT1M';MultipleInstances='IgnoreNew'}};Principal=@{UserId=$DesktopUser;LogonType='Interactive';RunLevel='Limited'}} $context",
        "firewall-failure-rolls-back",
        "the previous task definition is not restored verbatim",
    ),
    (
        "reinstall-regenerates-the-token",
        "  if(-not $previous.Config){\n   $bytes=New-Object byte[] 32",
        "  if($true){\n   $bytes=New-Object byte[] 32",
        "reinstall-preserves-token-and-replaces-definition",
        "reinstall replaces the existing config/token",
    ),
    (
        "postcheck-problems-are-only-reported",
        "  if($health.Problems.Count -gt 0){\n   $report.Problems=@($health.Problems)\n   throw ('Install verification failed: ' + ($health.Problems -join '; '))\n  }",
        "  $report.Problems=@($health.Problems)",
        "postcheck-failure-rolls-back",
        "a failed health check is reported but not treated as a failure",
    ),
    (
        "previous-helper-is-never-restarted",
        "  $previous.HelperWasRunning=($previous.Processes.Count -gt 0)",
        "  $previous.HelperWasRunning=$false",
        "task-start-failure-rolls-back",
        "rollback leaves no helper running after the old one was stopped",
    ),
    (
        "rollback-does-not-restore-the-directory-acl",
        "    Invoke-Op $ops 'RestoreDirectoryAcl' @{Path=$directory;Sddl=$previous.Acl[$directory]} $context\n    $restoredAcl=Invoke-Op $ops 'GetDirectoryAcl' @{Path=$directory} $context",
        "    $restoredAcl=Invoke-Op $ops 'GetDirectoryAcl' @{Path=$directory} $context",
        "firewall-failure-rolls-back",
        "the tightened ACL is left in place after a rollback",
    ),
    (
        "no-rollback-snapshot-is-taken",
        "   if(Invoke-Op $ops 'FileExists' @{Path=$current} $context){\n    Invoke-Op $ops 'CopyFile' @{Source=$current;Destination=(Join-Path $rollback $name)} $context\n    $previous.Artifacts+=@($name)\n   }",
        "   if(Invoke-Op $ops 'FileExists' @{Path=$current} $context){\n    $previous.Artifacts+=@($name)\n   }",
        "copy-failure-rolls-back",
        "the previous binaries are never backed up, so a rollback cannot restore them",
    ),
    (
        "rollback-does-not-stop-the-new-helper",
        " foreach($process in $newHelpers){\n  try{Invoke-Op $ops 'StopProcess' @{ProcessId=$process.ProcessId} $context;Add-RollbackStep 'stop-new-helper' $true ('process ' + $process.ProcessId)}catch{Add-RollbackStep 'stop-new-helper' $false $_.Exception.Message}\n }",
        " foreach($process in @()){\n  try{Invoke-Op $ops 'StopProcess' @{ProcessId=$process.ProcessId} $context;Add-RollbackStep 'stop-new-helper' $true ('process ' + $process.ProcessId)}catch{Add-RollbackStep 'stop-new-helper' $false $_.Exception.Message}\n }",
        # A new helper only exists once the task has been started, so the
        # postcheck scenario is the one that can observe the leftover.
        "postcheck-failure-rolls-back",
        "the failed run's own helper survives the rollback",
    ),
    (
        "rollback-does-not-remove-a-config-it-created",
        "  }else{\n   if(Invoke-Op $ops 'FileExists' @{Path=$configPath} $context){Invoke-Op $ops 'RemoveFile' @{Path=$configPath} $context}\n   if(Invoke-Op $ops 'FileExists' @{Path=$configPath} $context){throw 'config.json created by this run is still present'}\n   Add-RollbackStep 'remove-new-config' $true 'no previous config existed'\n  }",
        "  }else{\n   Add-RollbackStep 'remove-new-config' $true 'no previous config existed'\n  }",
        "first-install-failing-after-config-removes-the-token",
        "a generated config/token is left behind on a rolled-back first install",
    ),
    (
        "task-is-started-before-the-health-check-can-fail-it",
        "  }else{\n   Invoke-Op $ops 'UnregisterTask' @{Name=$TaskName} $context\n   if(Invoke-Op $ops 'GetTask' @{Name=$TaskName} $context){throw 'this run''s scheduled task is still registered'}\n   Add-RollbackStep 'remove-new-task' $true 'no previous task existed'\n  }",
        "  }else{\n   Add-RollbackStep 'remove-new-task' $true 'no previous task existed'\n  }",
        "fresh-install-failure-removes-task-firewall-and-files",
        "a first install's task is left registered after a rollback",
    ),
    # --- AR-08 follow-up: the four defects the native parent run found ------
    (
        "adapter-parses-iso8601-with-timespan-parse",
        "     $interval=ConvertTo-TaskDuration ([string]$settings['RestartInterval']) 'RestartInterval'",
        "     $interval=[TimeSpan]::Parse([string]$settings['RestartInterval'])",
        "task-adapter-accepts-iso8601-durations",
        "the adapter uses TimeSpan.Parse on the producer's ISO-8601 PT1M duration, so the real RegisterTask path throws before any task is registered",
    ),
    (
        "firewall-snapshot-reads-service-off-the-application-filter",
        "    @('Service',$service,'Service'),",
        "    @('Service',$application,'Service'),",
        "firewall-adapter-captures-service-interface-and-security-filters",
        "the snapshot reads Service from Get-NetFirewallApplicationFilter, which has no such property on Windows, so a service-scoped rule is restored as any-service",
    ),
    (
        "firewall-snapshot-drops-the-security-filter",
        "   # EdgeTraversalPolicy is exposed on the rule on some builds and on the",
        "   foreach($dropKey in @('Authentication','Encryption','OverrideBlockRules','LocalUser','RemoteUser','RemoteMachine')){$parameters.Remove($dropKey)}\n   # EdgeTraversalPolicy is exposed on the rule on some builds and on the",
        "firewall-adapter-captures-service-interface-and-security-filters",
        "the snapshot reads the security filter but discards it, so authentication/encryption/local-user/remote-machine scope is lost on restore",
    ),
    (
        "firewall-snapshot-accepts-an-incomplete-read",
        "   if($missing.Count -gt 0){\n    throw ('The existing firewall rule ' + $rule.Name + ' could not be snapshotted exactly (unreadable: ' + ($missing -join ', ') + ')')",
        "   if($false){\n    throw ('The existing firewall rule ' + $rule.Name + ' could not be snapshotted exactly (unreadable: ' + ($missing -join ', ') + ')')",
        "firewall-adapter-fails-closed-when-a-filter-is-unreadable",
        "an unreadable filter is tolerated and a partial snapshot is returned as if it were exact",
    ),
    (
        "directory-state-is-read-after-staging-exists",
        "   $existed=[bool]$preMutationDirs[$directory]",
        "   $existed=[bool](Invoke-Op $ops 'DirectoryExists' @{Path=$directory} $context)",
        "install-directory-created-by-this-run-is-removed-on-rollback",
        "the install directory's existence is re-read after staging created it inside, so a directory this run created is treated as pre-existing and left behind",
    ),
    (
        "recursive-force-delete-when-removing-a-created-directory",
        "   if(-not (Test-Path -LiteralPath $a.Path)){return $true}\n   $children=@(Get-ChildItem -LiteralPath $a.Path -Force -ErrorAction Stop)\n   if($children.Count -gt 0){return $false}\n   Remove-Item -LiteralPath $a.Path -Force -ErrorAction Stop",
        "   if(-not (Test-Path -LiteralPath $a.Path)){return $true}\n   Remove-Item -LiteralPath $a.Path -Recurse -Force -ErrorAction Stop",
        "remove-directory-if-empty-refuses-unknown-content",
        "the directory removal is an unconditional recursive force delete, so content this run never wrote is destroyed",
    ),
    (
        "owned-directory-delete-is-not-scope-checked",
        "   if($leaf -notmatch '^\\.(staging|rollback)-[0-9]{8}-[0-9]{6}$'){",
        "   if($false){",
        "owned-directory-delete-is-scope-checked",
        "the recursive delete accepts any path, so a mis-targeted rollback can delete an unrelated directory tree",
    ),
]


def run_suite(source: Path, pwsh: str):
    proc = subprocess.run(
        [pwsh, "-NoLogo", "-NoProfile", "-File", str(source / TESTS)],
        capture_output=True,
        text=True,
        cwd=str(source),
    )
    output = proc.stdout + proc.stderr
    failed = [
        line.split(" ", 2)[1]
        for line in output.splitlines()
        if line.startswith("FAIL ")
    ]
    return proc.returncode, failed, output


def main() -> int:
    if len(sys.argv) != 4:
        print(__doc__)
        return 2
    source = Path(sys.argv[1]).resolve()
    pwsh = sys.argv[2]
    work = Path(sys.argv[3]).resolve()
    if work.exists():
        shutil.rmtree(work)
    work.mkdir(parents=True)

    results = []
    baseline_dir = work / "baseline"
    shutil.copytree(source, baseline_dir, ignore=shutil.ignore_patterns("__pycache__"))
    code, failed, output = run_suite(baseline_dir, pwsh)
    (work / "baseline.log").write_text(output)
    results.append(
        {
            "mutation": "baseline-unmutated",
            "detected": code == 0,
            "exitCode": code,
            "failedTests": failed,
            "expectedDetector": None,
        }
    )
    print(f"baseline: exit={code} failed={len(failed)}")
    if code != 0:
        print(output[-4000:])
        (work / "mutation-results.json").write_text(json.dumps(results, indent=1))
        return 1

    ok = True
    for index, (name, old, new, detector, defect) in enumerate(MUTATIONS, start=1):
        target = work / f"{index:02d}-{name}"
        shutil.copytree(source, target, ignore=shutil.ignore_patterns("__pycache__"))
        install = target / INSTALL
        text = install.read_text()
        if text.count(old) != 1:
            results.append(
                {
                    "mutation": name,
                    "defect": defect,
                    "detected": False,
                    "error": f"anchor matched {text.count(old)} times, expected exactly 1",
                }
            )
            print(f"{name}: ANCHOR PROBLEM ({text.count(old)} matches)")
            ok = False
            continue
        install.write_text(text.replace(old, new, 1))
        code, failed, output = run_suite(target, pwsh)
        (work / f"{index:02d}-{name}.log").write_text(output)
        detected = code != 0 and any(detector in f for f in failed)
        results.append(
            {
                "mutation": name,
                "defect": defect,
                "detected": detected,
                "exitCode": code,
                "failedTests": failed,
                "expectedDetector": detector,
            }
        )
        status = "DETECTED" if detected else ("SURVIVED" if code == 0 else "WRONG DETECTOR")
        print(f"{name}: exit={code} {status}")
        if not detected:
            ok = False
            print(f"  failed: {failed}")

    (work / "mutation-results.json").write_text(json.dumps(results, indent=1))
    print("ALL MUTATIONS DETECTED" if ok else "SOME MUTATIONS NOT DETECTED")
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main())
