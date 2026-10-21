# セキュリティポリシー

## 報告方法

脆弱性や個人情報の漏えいは、公開 Issue に書かず、GitHub の非公開レポートから報告してください。

<https://github.com/TommyZ-7/vrc-photo-manager/security/advisories/new>

- 対象: 最新リリースと `main`
- 対応は best-effort（返信の保証はありません）
- 修正後、GitHub Security Advisory で公開する

## 報告に含めてほしいもの

- 影響するバージョンまたは commit
- 再現手順
- 想定される影響

## 注意

VRChat のログ、XMP、DB にはユーザーID・表示名・ローカルパスが含まれます。添付前に除去してください。
設計上の方針は [docs/security-privacy.md](docs/security-privacy.md) を参照してください。
